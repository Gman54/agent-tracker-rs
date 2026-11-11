//! Quick name list: a fast list of the working folder (names, sizes and modified times, no
//! hashing) taken the moment an agent prompt arrives. The full hashed "before" snapshot can take
//! a long time on a big folder, and a fast agent may delete a file before that snapshot reaches
//! it. The quick list closes that gap for the "nothing got deleted by surprise" check.
use crate::db::Db;
use crate::event::{AgentEvent, EventType};
use crate::fs_observer::{extended_path, is_reparse_point, mtime_ms, normalize_path};
use crate::Result;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Stop listing after this many files...
pub const MAX_FILES: usize = 250_000;
/// ...or after this long, so a huge folder never holds things up.
pub const MAX_MS: u64 = 10_000;
/// How long the hook delivery waits for the list before answering (the hook gives up at 200 ms;
/// the list keeps going in the background either way).
pub const ACCEPT_WAIT_MS: u64 = 100;
/// Lists never claimed by a task are removed after a day.
const UNCLAIMED_KEEP_MS: i64 = 24 * 3600 * 1000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuickEntry {
    pub rel_path: String,
    pub size: u64,
    pub mtime: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QuickList {
    pub id: String,
    pub task_id: Option<String>,
    pub queue_id: Option<String>,
    /// Normalized folder path (same form as manifest roots).
    pub root: String,
    /// When the request reached Houndbit.
    pub received_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub file_count: usize,
    pub cut_short: bool,
    pub cut_reason: Option<String>,
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub entries: Vec<QuickEntry>,
}

impl QuickList {
    /// The list finished and covers the whole folder.
    pub fn complete(&self) -> bool {
        self.finished_at.is_some() && !self.cut_short && self.error.is_none()
    }

    /// Plain words for why the list can't be fully trusted, or None when it is complete.
    pub fn problem(&self) -> Option<String> {
        if let Some(e) = &self.error {
            return Some(format!("The quick name list taken when you sent the request didn't work ({e})"));
        }
        if self.finished_at.is_none() {
            return Some("The quick name list taken when you sent the request didn't finish".into());
        }
        if self.cut_short {
            return Some(format!(
                "The quick name list taken when you sent the request was cut short after {} files ({})",
                thousands(self.file_count),
                self.cut_reason.clone().unwrap_or_else(|| "it hit its limit".into())
            ));
        }
        None
    }
}

/// What a walk produced.
#[derive(Debug, Default)]
pub struct Walk {
    pub entries: Vec<QuickEntry>,
    pub cut_reason: Option<String>,
    pub error: Option<String>,
}

/// Lists files under `root` with the same rules as the full snapshot: Houndbit's own files are
/// skipped, links and junctions are never followed, and folders that resolve outside the root are
/// skipped. Stops at `max_files` or `max_ms`.
pub fn walk(root: &Path, excludes: &[PathBuf], max_files: usize, max_ms: u64) -> Walk {
    let mut w = Walk::default();
    let t0 = Instant::now();
    let canon_root = match std::fs::canonicalize(extended_path(root)) {
        Ok(c) => c,
        Err(e) => {
            w.error = Some(format!("couldn't open the folder: {e}"));
            return w;
        }
    };
    let excludes: Vec<PathBuf> = excludes.iter().map(|p| std::fs::canonicalize(extended_path(p)).unwrap_or_else(|_| p.clone())).collect();
    let mut stack: Vec<(PathBuf, String)> = vec![(canon_root.clone(), String::new())];
    while let Some((dir, rel_dir)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for child in rd.filter_map(|r| r.ok()) {
            if w.entries.len() >= max_files {
                w.cut_reason = Some(format!("it stops at {} files", thousands(max_files)));
                return w;
            }
            if t0.elapsed() >= Duration::from_millis(max_ms) {
                w.cut_reason = Some(format!("it stops after {} seconds", max_ms / 1000));
                return w;
            }
            let path = child.path();
            if excludes.iter().any(|x| path.starts_with(x)) {
                continue;
            }
            let name = child.file_name().to_string_lossy().into_owned();
            let rel = if rel_dir.is_empty() { name } else { format!("{rel_dir}/{name}") };
            let Ok(md) = std::fs::symlink_metadata(&path) else { continue };
            let ft = md.file_type();
            if ft.is_symlink() || (ft.is_dir() && is_reparse_point(&md)) {
                continue;
            }
            if ft.is_dir() {
                if matches!(std::fs::canonicalize(&path), Ok(c) if c.starts_with(&canon_root)) {
                    stack.push((path, rel));
                }
                continue;
            }
            if ft.is_file() {
                w.entries.push(QuickEntry { rel_path: rel, size: md.len(), mtime: mtime_ms(&md) });
            }
        }
    }
    w
}

/// Records that a request arrived for `root` (before the list is taken). Returns the list id.
pub fn begin(db: &Db, queue_id: &str, root: &Path, received_at: i64) -> Result<String> {
    let id = uuid::Uuid::new_v4().to_string();
    let conn = db.conn();
    conn.execute("DELETE FROM quick_name_lists WHERE task_id IS NULL AND received_at < ?1", [received_at - UNCLAIMED_KEEP_MS])?;
    conn.execute(
        "INSERT INTO quick_name_lists(id, queue_id, root, received_at) VALUES (?1,?2,?3,?4)",
        params![id, queue_id, normalize_path(root), received_at],
    )?;
    Ok(id)
}

/// Stores the finished walk.
pub fn finish(db: &Db, id: &str, started_at: i64, w: &Walk) -> Result<()> {
    db.conn().execute(
        "UPDATE quick_name_lists SET started_at=?2, finished_at=?3, file_count=?4, cut_short=?5, cut_reason=?6, error=?7, entries=?8 WHERE id=?1",
        params![
            id,
            started_at,
            crate::now_ms(),
            w.entries.len() as i64,
            w.cut_reason.is_some(),
            w.cut_reason,
            w.error,
            serde_json::to_string(&w.entries)?
        ],
    )?;
    Ok(())
}

/// Takes the list now (on a background thread) and waits up to `wait_ms` for it.
pub fn take(db: &Db, queue_id: &str, root: &Path, excludes: Vec<PathBuf>, received_at: i64, wait_ms: u64) -> Result<String> {
    let id = begin(db, queue_id, root, received_at)?;
    let (db2, id2, root2) = (db.clone(), id.clone(), root.to_path_buf());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let started = crate::now_ms();
        let w = walk(&root2, &excludes, MAX_FILES, MAX_MS);
        if let Err(e) = finish(&db2, &id2, started, &w) {
            eprintln!("quick name list couldn't be saved: {e}");
        }
        let _ = tx.send(());
    });
    let _ = rx.recv_timeout(Duration::from_millis(wait_ms));
    Ok(id)
}

/// Links the list taken for a queued request to the task that request started.
pub fn claim(db: &Db, queue_id: &str, task_id: &str) -> Result<usize> {
    Ok(db.conn().execute("UPDATE quick_name_lists SET task_id=?2 WHERE queue_id=?1 AND task_id IS NULL", params![queue_id, task_id])?)
}

fn row(r: &rusqlite::Row, with_entries: bool) -> rusqlite::Result<QuickList> {
    let entries: Option<String> = if with_entries { r.get(11)? } else { None };
    Ok(QuickList {
        id: r.get(0)?,
        queue_id: r.get(1)?,
        task_id: r.get(2)?,
        root: r.get(3)?,
        received_at: r.get(4)?,
        started_at: r.get(5)?,
        finished_at: r.get(6)?,
        file_count: r.get::<_, i64>(7)? as usize,
        cut_short: r.get(8)?,
        cut_reason: r.get(9)?,
        error: r.get(10)?,
        entries: entries.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default(),
    })
}

const COLS: &str = "id, queue_id, task_id, root, received_at, started_at, finished_at, file_count, cut_short, cut_reason, error, entries";

/// Every quick list for a task (with entries), oldest first.
pub fn for_task(db: &Db, task_id: &str) -> Result<Vec<QuickList>> {
    let conn = db.conn();
    let mut st = conn.prepare(&format!("SELECT {COLS} FROM quick_name_lists WHERE task_id=?1 ORDER BY received_at"))?;
    let v = st.query_map([task_id], |r| row(r, true))?.collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(v)
}

/// The first quick list for a task, without its entries (for the report's timeline).
pub fn summary_for_task(db: &Db, task_id: &str) -> Result<Option<QuickList>> {
    let conn = db.conn();
    Ok(conn.query_row(&format!("SELECT {COLS} FROM quick_name_lists WHERE task_id=?1 ORDER BY received_at LIMIT 1"), [task_id], |r| row(r, false)).optional()?)
}

/// When the full "before" snapshot started and finished.
pub fn record_baseline_times(db: &Db, task_id: &str, started_at: i64, finished_at: i64) -> Result<()> {
    db.conn().execute(
        "INSERT INTO task_snapshot_times(task_id, baseline_started_at, baseline_finished_at) VALUES (?1,?2,?3)
         ON CONFLICT(task_id) DO UPDATE SET baseline_started_at=excluded.baseline_started_at, baseline_finished_at=excluded.baseline_finished_at",
        params![task_id, started_at, finished_at],
    )?;
    Ok(())
}

pub fn baseline_times(db: &Db, task_id: &str) -> Result<Option<(i64, i64)>> {
    Ok(db
        .conn()
        .query_row("SELECT baseline_started_at, baseline_finished_at FROM task_snapshot_times WHERE task_id=?1", [task_id], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?)
}

/// When the agent first used a tool (its first action), if any was reported.
pub fn first_action_at(events: &[AgentEvent]) -> Option<i64> {
    events
        .iter()
        .filter(|e| matches!(e.event_type, EventType::ToolStarted | EventType::ToolFinished))
        .map(|e| e.source_ts.unwrap_or(e.collector_ts))
        .min()
}

/// 17904 -> "17,904".
pub fn thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The note shown when the agent got going before the full snapshot was done.
pub const EARLY_START_NOTE: &str = "The agent started working before the full snapshot finished; the quick name list taken when you sent the request was used to spot deleted files.";
pub const EARLY_START_NO_LIST_NOTE: &str =
    "The agent started working before the full snapshot finished, and there was no complete quick name list from when you sent the request, so a file deleted very early might not have been noticed.";

/// The times behind a task, for the report: when the request arrived, the quick name list, the
/// full "before" snapshot and the agent's first action, plus a plain note when they overlapped.
pub fn timeline(quick: Option<&QuickList>, baseline: Option<(i64, i64)>, first_action: Option<i64>) -> Value {
    let mut rows: Vec<Value> = vec![];
    let mut push = |label: &str, at: Option<i64>, text: String| rows.push(json!({"label": label, "at": at, "text": text}));
    if let Some(q) = quick {
        push("Your request arrived", Some(q.received_at), String::new());
        let text = match (&q.error, q.finished_at) {
            (Some(e), _) => format!("Couldn't be taken: {e}"),
            (None, None) => "Started, but didn't finish".into(),
            (None, Some(_)) if q.cut_short => format!(
                "{} files listed, then cut short ({})",
                thousands(q.file_count),
                q.cut_reason.clone().unwrap_or_else(|| "it hit its limit".into())
            ),
            (None, Some(f)) => format!("{} files listed in {}", thousands(q.file_count), took(q.started_at.unwrap_or(q.received_at), f)),
        };
        push("Quick name list taken", q.started_at.or(Some(q.received_at)), text);
    }
    if let Some((s, f)) = baseline {
        push("Full \"before\" snapshot started", Some(s), String::new());
        push("Full \"before\" snapshot finished", Some(f), format!("Took {}", took(s, f)));
    }
    match first_action {
        Some(a) => push("The agent's first action", Some(a), String::new()),
        None => push("The agent's first action", None, "Not known: the agent didn't report using any tools".into()),
    }
    let early = matches!((baseline, first_action), (Some((_, f)), Some(a)) if a < f);
    let note = if early {
        if quick.map(QuickList::complete).unwrap_or(false) {
            Some(EARLY_START_NOTE)
        } else {
            Some(EARLY_START_NO_LIST_NOTE)
        }
    } else {
        None
    };
    json!({"rows": rows, "note": note, "agent_started_early": early})
}

fn took(a: i64, b: i64) -> String {
    let ms = (b - a).max(0);
    if ms < 1000 {
        format!("{ms} ms")
    } else if ms < 120_000 {
        format!("{:.1} seconds", ms as f64 / 1000.0)
    } else {
        format!("{} minutes", ms / 60_000)
    }
}

/// Builds the timeline for a stored task.
pub fn timeline_for_task(db: &Db, task_id: &str, events: &[AgentEvent]) -> Value {
    let q = summary_for_task(db, task_id).ok().flatten();
    let b = baseline_times(db, task_id).ok().flatten();
    timeline(q.as_ref(), b, first_action_at(events))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walk_lists_files_and_stops_at_the_cap() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("sub")).unwrap();
        for i in 0..5 {
            std::fs::write(d.path().join(format!("f{i}.txt")), "x").unwrap();
        }
        std::fs::write(d.path().join("sub/inner.txt"), "yy").unwrap();
        let w = walk(d.path(), &[], 1000, 10_000);
        assert_eq!(w.entries.len(), 6);
        assert!(w.cut_reason.is_none() && w.error.is_none());
        assert!(w.entries.iter().any(|e| e.rel_path == "sub/inner.txt" && e.size == 2));
        let w = walk(d.path(), &[], 3, 10_000);
        assert_eq!(w.entries.len(), 3);
        assert_eq!(w.cut_reason.as_deref(), Some("it stops at 3 files"));
        let w = walk(&d.path().join("missing"), &[], 10, 10_000);
        assert!(w.error.is_some());
    }

    #[test]
    fn thousands_and_timeline_note() {
        assert_eq!(thousands(17904), "17,904");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000_000), "1,000,000");
        let q = QuickList { received_at: 1_000, started_at: Some(1_001), finished_at: Some(1_300), file_count: 17904, ..Default::default() };
        let t = timeline(Some(&q), Some((1_400, 60_000)), Some(5_000));
        assert_eq!(t["note"], EARLY_START_NOTE);
        assert!(t["rows"][1]["text"].as_str().unwrap().starts_with("17,904 files listed"));
        let t = timeline(Some(&q), Some((1_400, 2_000)), Some(5_000));
        assert!(t["note"].is_null());
        let cut = QuickList { cut_short: true, ..q.clone() };
        assert_eq!(timeline(Some(&cut), Some((1_400, 60_000)), Some(5_000))["note"], EARLY_START_NO_LIST_NOTE);
    }
}
