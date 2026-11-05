//! Compares what agents were seen editing with what a commit actually contains, and works out
//! four separate facts (who edited, who committed, did the agent push, did the remote update).
//! Every conclusion carries its basis and a confidence; missing evidence stays "unknown".
use super::cli;
use super::github::GithubInfo;
use super::Repo;
use crate::db::Db;
use crate::Result;
use serde::Serialize;
use serde_json::Value;
use similar::{ChangeTag, TextDiff};
use std::path::Path;

/// Seconds of slack between an observed event and a commit timestamp (git stores whole seconds).
const SLACK_MS: i64 = 2_000;

#[derive(Debug, Clone, Serialize)]
pub struct CommitInfo {
    pub sha: String,
    pub short: String,
    pub subject: String,
    pub parents: Vec<String>,
    pub author_name: String,
    pub author_email: String,
    pub author_time: i64,
    pub committer_name: String,
    pub committer_email: String,
    pub commit_time: i64,
}

const LOG_FMT: &str = "--format=%H%x1f%P%x1f%an%x1f%ae%x1f%at%x1f%cn%x1f%ce%x1f%ct%x1f%s%x1e";

fn parse_log(out: &str) -> Vec<CommitInfo> {
    out.split('\x1e')
        .filter_map(|rec| {
            let f: Vec<&str> = rec.trim_start_matches('\n').split('\x1f').collect();
            if f.len() < 9 || f[0].len() < 7 {
                return None;
            }
            Some(CommitInfo {
                sha: f[0].to_string(),
                short: f[0][..7].to_string(),
                parents: f[1].split_whitespace().map(str::to_string).collect(),
                author_name: f[2].into(),
                author_email: f[3].into(),
                author_time: f[4].parse().unwrap_or(0),
                committer_name: f[5].into(),
                committer_email: f[6].into(),
                commit_time: f[7].parse().unwrap_or(0),
                subject: f[8].trim_end().into(),
            })
        })
        .collect()
}

pub fn list_commits(root: &Path, limit: usize) -> Result<Vec<CommitInfo>> {
    let n = format!("-n{}", limit.clamp(1, 500));
    match cli::run(root, &["log", &n, LOG_FMT]) {
        Ok(out) => Ok(parse_log(&out)),
        // A brand-new repository has no commits yet.
        Err(e) if e.to_string().contains("does not have any commits") || e.to_string().contains("bad default revision") => Ok(vec![]),
        Err(e) => Err(e),
    }
}

pub fn commit_info(root: &Path, rev: &str) -> Result<CommitInfo> {
    if rev.starts_with('-') || rev.contains("..") {
        return Err(crate::CoreError::Invalid("That isn't a commit id.".into()));
    }
    let out = cli::run(root, &["log", "-n1", LOG_FMT, rev, "--"])?;
    parse_log(&out).into_iter().next().ok_or_else(|| crate::CoreError::NotFound(format!("commit {rev}")))
}

#[derive(Debug, Clone, Serialize)]
pub struct DiffLine {
    pub kind: &'static str, // add | del | ctx | gap
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShippedFile {
    pub path: String,
    pub old_path: Option<String>,
    pub status: String, // added | modified | deleted | renamed
    pub binary: bool,
    pub added: usize,
    pub removed: usize,
    pub diff: Vec<DiffLine>,
    pub blob: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DecisionRecord {
    pub decision_id: String,
    pub source: String, // hook_event | runtime_decision
    pub source_label: String,
    pub event_id: Option<String>,
    pub session_id: Option<String>,
    pub task_id: Option<String>,
    pub task_title: Option<String>,
    pub connector: String,
    pub tool: String,
    pub observed_at: i64,
    /// UI route that opens the record ("#results/<task>" or "#runtime/<decision>").
    pub open: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RewrittenLine {
    pub proposed: String,
    pub shipped: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct EditMatch {
    pub edit_id: String,
    pub path: String,
    pub decision: DecisionRecord,
    /// shipped_as_is | shipped | revised | not_staged | staged_not_committed | later_commit | not_shipped | unknown
    pub outcome: String,
    pub outcome_label: String,
    pub confidence: &'static str, // high | medium | low
    pub basis: Vec<String>,
    /// What the agent proposed (its own change, as a diff).
    pub proposed: Vec<DiffLine>,
    pub agent_lines_shipped: Vec<String>,
    pub agent_lines_reworded: Vec<RewrittenLine>,
    pub agent_lines_not_shipped: Vec<String>,
    pub agent_removals_kept: Vec<String>,
    pub before_evidence: Value,
    pub after_evidence: Value,
    pub changed_after_agent: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OtherSource {
    pub path: String,
    pub lines: usize,
    pub sample: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Fact {
    /// confirmed | likely | no | not_observed | unknown
    pub status: &'static str,
    pub label: String,
    pub evidence: Vec<String>,
    #[serde(skip_serializing_if = "Value::is_null")]
    pub detail: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct Facts {
    pub edit_attribution: Fact,
    pub commit_authorship: Fact,
    pub agent_push: Fact,
    pub remote_update: Fact,
}

#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub lines_added: usize,
    pub agent_lines_as_proposed: usize,
    pub agent_lines_reworded: usize,
    pub other_lines: usize,
    pub files: usize,
    pub files_with_agent_edits: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct CommitReport {
    pub repo: Repo,
    pub commit: CommitInfo,
    pub files: Vec<ShippedFile>,
    pub edits: Vec<EditMatch>,
    pub not_in_commit: Vec<EditMatch>,
    pub other_sources: Vec<OtherSource>,
    pub summary: Summary,
    pub facts: Facts,
    pub github: Option<GithubInfo>,
    pub privacy: String,
}

// ---- small text helpers -----------------------------------------------------------------------------

fn split_lines(s: &str) -> Vec<String> {
    s.split('\n').map(|l| l.trim_end_matches('\r').to_string()).collect()
}

fn meaningful(l: &str) -> bool {
    !l.trim().is_empty()
}

/// (added, removed) lines between two texts.
fn line_changes(old: &str, new: &str) -> (Vec<String>, Vec<String>) {
    let d = TextDiff::from_lines(old, new);
    let (mut a, mut r) = (vec![], vec![]);
    for c in d.iter_all_changes() {
        let t = c.value().trim_end_matches(['\n', '\r']).to_string();
        match c.tag() {
            ChangeTag::Insert => a.push(t),
            ChangeTag::Delete => r.push(t),
            ChangeTag::Equal => {}
        }
    }
    (a, r)
}

fn diff_view(old: &str, new: &str, ctx: usize) -> Vec<DiffLine> {
    let d = TextDiff::from_lines(old, new);
    let mut out = vec![];
    for (i, group) in d.grouped_ops(ctx).iter().enumerate() {
        if i > 0 {
            out.push(DiffLine { kind: "gap", text: String::new() });
        }
        for op in group {
            for c in d.iter_changes(op) {
                let kind = match c.tag() {
                    ChangeTag::Insert => "add",
                    ChangeTag::Delete => "del",
                    ChangeTag::Equal => "ctx",
                };
                out.push(DiffLine { kind, text: c.value().trim_end_matches(['\n', '\r']).to_string() });
            }
        }
        if out.len() > 400 {
            out.push(DiffLine { kind: "gap", text: "(more changes not shown)".into() });
            break;
        }
    }
    out
}

fn take_one(pool: &mut Vec<String>, line: &str) -> bool {
    if let Some(i) = pool.iter().position(|l| l == line) {
        pool.remove(i);
        true
    } else {
        false
    }
}

fn similarity(a: &str, b: &str) -> f32 {
    TextDiff::from_chars(a.trim(), b.trim()).ratio()
}

fn contains_all(haystack: &[String], needles: &[String]) -> bool {
    let mut pool = haystack.to_vec();
    !needles.is_empty() && needles.iter().all(|n| take_one(&mut pool, n))
}

// ---- recorded agent edits ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct EditRow {
    id: String,
    decision_id: String,
    decision_source: String,
    event_id: Option<String>,
    session_id: Option<String>,
    task_id: Option<String>,
    connector: String,
    tool: String,
    rel_path: String,
    proposal: Value,
    before_source: String,
    before_file: Option<String>,
    before_blob: Option<String>,
    after_file: Option<String>,
    after_blob: Option<String>,
    head_at_edit: Option<String>,
    user_modified: Option<bool>,
    observed_at: i64,
}

fn edits_for_repo(db: &Db, repo: &Repo, until_ms: i64) -> Result<Vec<EditRow>> {
    let conn = db.conn();
    let mut st = conn.prepare(
        "SELECT id, decision_id, decision_source, event_id, session_id, task_id, connector, tool, rel_path, proposal, before_source, before_file, before_blob,
                after_file, after_blob, head_at_edit, user_modified, observed_at, repo_root, repo_identity
           FROM git_agent_edits WHERE observed_at <= ?1 ORDER BY observed_at, id",
    )?;
    let rows = st.query_map([until_ms], |r| {
        Ok((
            EditRow {
                id: r.get(0)?,
                decision_id: r.get(1)?,
                decision_source: r.get(2)?,
                event_id: r.get(3)?,
                session_id: r.get(4)?,
                task_id: r.get(5)?,
                connector: r.get(6)?,
                tool: r.get(7)?,
                rel_path: r.get(8)?,
                proposal: serde_json::from_str(&r.get::<_, String>(9)?).unwrap_or(Value::Null),
                before_source: r.get(10)?,
                before_file: r.get(11)?,
                before_blob: r.get(12)?,
                after_file: r.get(13)?,
                after_blob: r.get(14)?,
                head_at_edit: r.get(15)?,
                user_modified: r.get(16)?,
                observed_at: r.get(17)?,
            },
            r.get::<_, String>(18)?,
            r.get::<_, String>(19)?,
        ))
    })?;
    let mut out = vec![];
    for row in rows {
        let (e, root, identity) = row?;
        if identity == repo.identity || cli::same_root(&root, &repo.root) {
            out.push(e);
        }
    }
    Ok(out)
}

/// The agent's own change: (added, removed, diff view, whether it is based on its exact request).
fn agent_change(e: &EditRow) -> (Vec<String>, Vec<String>, Vec<DiffLine>, bool) {
    let props = e.proposal.as_array().cloned().unwrap_or_default();
    let (mut a, mut r, mut view) = (vec![], vec![], vec![]);
    let mut exact_request = !props.is_empty();
    // The file as it was before this proposal, kept up to date across a multi-part edit.
    let mut cur = e.before_file.clone();
    for (i, p) in props.iter().enumerate() {
        let new = p.get("new").and_then(Value::as_str).unwrap_or("");
        let whole = p.get("whole_file").and_then(Value::as_bool).unwrap_or(false);
        let (old_full, new_full) = if whole {
            if e.before_file.is_none() && e.before_source == "none" {
                exact_request = false; // we don't know what the file held before a whole-file write
            }
            let o = cur.clone().unwrap_or_default();
            cur = Some(new.to_string());
            (o, new.to_string())
        } else {
            let old = p.get("old").and_then(Value::as_str).unwrap_or("");
            // Edit tools send fragments ("10" -> "25"); widen them to whole lines so they compare with Git's lines.
            let widened = cur.as_deref().and_then(|t| widen(t, old, new, true)).or_else(|| e.after_file.as_deref().and_then(|t| widen(t, old, new, false)));
            if let Some(t) = cur.as_mut() {
                if !old.is_empty() && t.contains(old) {
                    *t = t.replacen(old, new, 1);
                }
            }
            widened.unwrap_or_else(|| (old.to_string(), new.to_string()))
        };
        let (aa, rr) = line_changes(&ensure_nl(&old_full), &ensure_nl(&new_full));
        a.extend(aa);
        r.extend(rr);
        if i > 0 {
            view.push(DiffLine { kind: "gap", text: String::new() });
        }
        view.extend(diff_view(&ensure_nl(&old_full), &ensure_nl(&new_full), if whole { 2 } else { 50 }));
    }
    if props.is_empty() {
        if let (Some(b), Some(af)) = (&e.before_file, &e.after_file) {
            let (aa, rr) = line_changes(b, af);
            a = aa;
            r = rr;
            view = diff_view(b, af, 2);
        }
    }
    a.retain(|l| meaningful(l));
    r.retain(|l| meaningful(l));
    (a, r, view, exact_request)
}

/// Widens a fragment replacement to the full lines around it. `text` holds `old` (when
/// `in_before`) or `new` (otherwise).
fn widen(text: &str, old: &str, new: &str, in_before: bool) -> Option<(String, String)> {
    let needle = if in_before { old } else { new };
    if needle.is_empty() {
        return None;
    }
    let pos = text.find(needle)?;
    let start = text[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let end_from = pos + needle.len();
    let end = if needle.ends_with('\n') { end_from } else { text[end_from..].find('\n').map(|i| end_from + i).unwrap_or(text.len()) };
    let (prefix, suffix) = (&text[start..pos], &text[end_from..end]);
    Some((format!("{prefix}{old}{suffix}"), format!("{prefix}{new}{suffix}")))
}

fn ensure_nl(s: &str) -> String {
    if s.is_empty() || s.ends_with('\n') {
        s.to_string()
    } else {
        format!("{s}\n")
    }
}

fn task_title(db: &Db, id: &str) -> Option<String> {
    db.conn().query_row("SELECT title FROM tasks WHERE id=?1", [id], |r| r.get(0)).ok()
}

fn decision_record(db: &Db, e: &EditRow) -> DecisionRecord {
    let runtime = e.decision_source == "runtime_decision";
    DecisionRecord {
        decision_id: e.decision_id.clone(),
        source: e.decision_source.clone(),
        source_label: if runtime { "Agent Runtime decision".into() } else { "Agent tool call (from hooks)".into() },
        event_id: e.event_id.clone(),
        session_id: e.session_id.clone(),
        task_title: e.task_id.as_deref().and_then(|t| task_title(db, t)),
        task_id: e.task_id.clone(),
        connector: e.connector.clone(),
        tool: e.tool.clone(),
        observed_at: e.observed_at,
        open: if runtime { Some(format!("#runtime/{}", e.decision_id)) } else { e.task_id.as_ref().map(|t| format!("#results/{t}")) },
    }
}

fn evidence_json(e: &EditRow) -> (Value, Value) {
    let before_label = match e.before_source.as_str() {
        "pre_hook_snapshot" => "Read by AgentTracker just before the agent's tool ran",
        "agent_report" => "The original file as the agent reported it",
        _ => "Not available",
    };
    (
        serde_json::json!({"source": e.before_source, "label": before_label, "blob": e.before_blob, "head_at_edit": e.head_at_edit, "has_text": e.before_file.is_some()}),
        serde_json::json!({"blob": e.after_blob, "label": if e.after_blob.is_some() { "Read by AgentTracker right after the edit" } else { "The file couldn't be read after the edit" }, "has_text": e.after_file.is_some()}),
    )
}

// ---- the comparison ---------------------------------------------------------------------------------

struct FileSide {
    path: String,
    old_path: Option<String>,
    status: String,
    parent_text: Option<String>,
    commit_text: Option<String>,
    blob: Option<String>,
    binary: bool,
}

fn changed_files(root: &Path, c: &CommitInfo) -> Result<Vec<FileSide>> {
    let out = match c.parents.first() {
        Some(p) => cli::run_bytes(root, &["diff-tree", "-r", "-M", "-z", "--no-commit-id", "--name-status", p, &c.sha])?,
        None => cli::run_bytes(root, &["diff-tree", "-r", "-M", "-z", "--no-commit-id", "--name-status", "--root", &c.sha])?,
    };
    let s = String::from_utf8_lossy(&out).into_owned();
    let mut toks = s.split('\0').filter(|t| !t.is_empty());
    let mut files = vec![];
    while let Some(st) = toks.next() {
        let code = st.chars().next().unwrap_or('M');
        let (path, old_path) = if code == 'R' || code == 'C' {
            let old = toks.next().unwrap_or("").to_string();
            (toks.next().unwrap_or("").to_string(), Some(old))
        } else {
            (toks.next().unwrap_or("").to_string(), None)
        };
        let status = match code {
            'A' => "added",
            'D' => "deleted",
            'R' => "renamed",
            'C' => "copied",
            _ => "modified",
        }
        .to_string();
        let parent_rev = c.parents.first().map(|p| format!("{p}:{}", old_path.as_deref().unwrap_or(&path)));
        let parent_bytes = parent_rev.and_then(|r| cli::run_bytes(root, &["cat-file", "blob", &r]).ok());
        let commit_bytes = if status == "deleted" { None } else { cli::run_bytes(root, &["cat-file", "blob", &format!("{}:{path}", c.sha)]).ok() };
        let binary = parent_bytes.as_deref().map(|b| cli::as_text(b).is_none()).unwrap_or(false) || commit_bytes.as_deref().map(|b| cli::as_text(b).is_none()).unwrap_or(false);
        files.push(FileSide {
            blob: commit_bytes.as_deref().map(cli::blob_id),
            parent_text: parent_bytes.as_deref().and_then(cli::as_text).map(|t| cli::scrub(&t)),
            commit_text: commit_bytes.as_deref().and_then(cli::as_text).map(|t| cli::scrub(&t)),
            path,
            old_path,
            status,
            binary,
        });
    }
    Ok(files)
}

fn read_index(root: &Path, path: &str) -> Option<String> {
    cli::run_bytes(root, &["cat-file", "blob", &format!(":{path}")]).ok().and_then(|b| cli::as_text(&b)).map(|t| cli::scrub(&t))
}

fn read_head(root: &Path, path: &str) -> Option<String> {
    cli::run_bytes(root, &["cat-file", "blob", &format!("HEAD:{path}")]).ok().and_then(|b| cli::as_text(&b)).map(|t| cli::scrub(&t))
}

fn read_work(root: &Path, path: &str) -> Option<String> {
    std::fs::read(root.join(path)).ok().and_then(|b| cli::as_text(&b)).map(|t| cli::scrub(&t))
}

/// Where an agent change that isn't in the commit is now.
fn where_now(root: &Path, head: Option<&str>, sha: &str, path: &str, added: &[String]) -> (&'static str, String) {
    let has = |t: Option<String>| t.map(|t| contains_all(&split_lines(&t), added)).unwrap_or(false);
    if head.map(|h| h != sha).unwrap_or(false) && has(read_head(root, path)) {
        return ("later_commit", "Not in this commit, but it is in a later commit.".into());
    }
    if has(read_index(root, path)) {
        return ("staged_not_committed", "Not in this commit. It's staged now but not committed yet.".into());
    }
    if has(read_work(root, path)) {
        return ("not_staged", "Not in this commit. It's still in the working folder but wasn't staged.".into());
    }
    ("not_shipped", "Not in this commit and no longer in the folder. It was probably undone or replaced.".into())
}

fn label_for(outcome: &str) -> &'static str {
    match outcome {
        "shipped_as_is" => "Shipped exactly as the agent left it",
        "shipped" => "Shipped as the agent proposed",
        "revised" => "Changed by someone before it was committed",
        "not_staged" => "Not in this commit (not staged)",
        "staged_not_committed" => "Not in this commit (staged, not committed)",
        "later_commit" => "Not in this commit (in a later one)",
        "earlier_commit" => "Already committed earlier",
        "not_shipped" => "Didn't ship",
        _ => "Can't tell",
    }
}

#[allow(clippy::too_many_arguments)]
fn match_edit(db: &Db, root: &Path, head: Option<&str>, c: &CommitInfo, e: &EditRow, f: Option<&FileSide>, latest_on_file: bool, unexplained: &mut Vec<String>) -> Option<EditMatch> {
    let (added, removed, proposed, exact_request) = agent_change(e);
    let (before_ev, after_ev) = evidence_json(e);
    let mut m = EditMatch {
        edit_id: e.id.clone(),
        path: e.rel_path.clone(),
        decision: decision_record(db, e),
        outcome: String::new(),
        outcome_label: String::new(),
        confidence: "low",
        basis: vec![],
        proposed,
        agent_lines_shipped: vec![],
        agent_lines_reworded: vec![],
        agent_lines_not_shipped: vec![],
        agent_removals_kept: vec![],
        before_evidence: before_ev,
        after_evidence: after_ev,
        changed_after_agent: None,
    };
    let set = |m: &mut EditMatch, o: &str, conf: &'static str| {
        m.outcome = o.into();
        m.outcome_label = label_for(o).into();
        m.confidence = conf;
    };
    if e.user_modified == Some(true) {
        m.basis.push("The agent reported that you changed its proposal before it was applied.".into());
    }

    let Some(f) = f else {
        // File not in this commit at all.
        if added.is_empty() {
            return None;
        }
        let parent_has = c.parents.first().and_then(|p| cli::run_bytes(root, &["cat-file", "blob", &format!("{p}:{}", e.rel_path)]).ok()).and_then(|b| cli::as_text(&b)).map(|t| contains_all(&split_lines(&cli::scrub(&t)), &added)).unwrap_or(false);
        if parent_has {
            return None; // committed before this one
        }
        let (o, why) = where_now(root, head, &c.sha, &e.rel_path, &added);
        set(&mut m, o, "medium");
        m.basis.push(why);
        m.agent_lines_not_shipped = added;
        return Some(m);
    };

    let commit_lines = f.commit_text.as_deref().map(split_lines).unwrap_or_default();
    let parent_lines = f.parent_text.as_deref().map(split_lines).unwrap_or_default();
    let (mut c_added, c_removed) = line_changes(f.parent_text.as_deref().unwrap_or(""), f.commit_text.as_deref().unwrap_or(""));
    c_added.retain(|l| meaningful(l));

    if latest_on_file {
        if let (Some(a), Some(b)) = (&e.after_blob, &f.blob) {
            m.changed_after_agent = Some(a != b);
        }
    }

    // Exact: the committed file is byte-for-byte what the agent left behind.
    if latest_on_file && e.after_blob.is_some() && e.after_blob == f.blob {
        set(&mut m, "shipped_as_is", "high");
        m.basis.push(format!("The file in this commit has the same Git blob id ({}) as the file AgentTracker read right after the agent's edit.", &f.blob.clone().unwrap_or_default()[..12.min(f.blob.as_deref().unwrap_or("").len())]));
        m.agent_lines_shipped = added;
        return Some(m);
    }

    if f.binary {
        set(&mut m, "unknown", "low");
        m.basis.push("This is a binary or very large file, so lines can't be compared.".into());
        return Some(m);
    }
    if added.is_empty() && removed.is_empty() {
        set(&mut m, "unknown", "low");
        m.basis.push("The agent said it edited this file but didn't report what it changed, and the file in the commit isn't the one it left.".into());
        return Some(m);
    }
    // Already in the parent and not touched again: it belongs to an earlier commit.
    let mut pool = c_added.clone();
    let shipped: Vec<String> = added.iter().filter(|l| take_one(&mut pool, l)).cloned().collect();
    if shipped.is_empty() && !added.is_empty() && contains_all(&parent_lines, &added) {
        return None;
    }
    let missing: Vec<String> = added.iter().filter(|l| !shipped.contains(l)).cloned().collect();
    // Lines that were reworded: a committed line nobody else explains, very similar to the agent's.
    let mut reworded = vec![];
    let mut not_shipped = vec![];
    for mline in &missing {
        let best = unexplained.iter().enumerate().map(|(i, c)| (i, similarity(mline, c))).filter(|(_, s)| *s >= 0.6).max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        if let Some((i, _)) = best {
            let shipped_line = unexplained.remove(i);
            reworded.push(RewrittenLine { proposed: mline.clone(), shipped: shipped_line });
        } else {
            not_shipped.push(mline.clone());
        }
    }
    let removals_done = removed.iter().filter(|r| c_removed.contains(r) || !commit_lines.contains(r)).count();
    let removals_kept: Vec<String> = removed.iter().filter(|r| !(c_removed.contains(r) || !commit_lines.contains(r))).cloned().collect();
    let total = added.len() + removed.len();
    let got = shipped.len() + removals_done;

    m.agent_lines_shipped = shipped.clone();
    m.agent_lines_reworded = reworded.clone();
    m.agent_lines_not_shipped = not_shipped.clone();
    m.agent_removals_kept = removals_kept.clone();

    if got == total {
        set(&mut m, "shipped", if exact_request { "high" } else { "medium" });
        if !added.is_empty() {
            m.basis.push(format!("All {} line(s) the agent added are among the lines this commit added.", added.len()));
        }
        if !removed.is_empty() {
            m.basis.push(format!("All {} line(s) the agent removed are gone in this commit.", removed.len()));
        }
        if !exact_request {
            m.basis.push("The agent's exact request isn't fully known, so this is a line comparison only.".into());
        }
    } else if got > 0 || !reworded.is_empty() {
        set(&mut m, "revised", if exact_request { "high" } else { "medium" });
        m.basis.push(format!("{got} of {total} line change(s) shipped as the agent proposed."));
        if !reworded.is_empty() {
            m.basis.push(format!("{} line(s) were reworded before commit (matched by similarity, not exact text).", reworded.len()));
        }
        if !not_shipped.is_empty() {
            m.basis.push(format!("{} line(s) the agent added aren't in this commit.", not_shipped.len()));
            if contains_all(&read_work(root, &e.rel_path).map(|t| split_lines(&t)).unwrap_or_default(), &not_shipped) {
                m.basis.push("Those lines are still in the working folder, so only part of the change was staged.".into());
            }
        }
        if !removals_kept.is_empty() {
            m.basis.push(format!("{} line(s) the agent removed are still in the committed file.", removals_kept.len()));
        }
    } else {
        let (o, why) = where_now(root, head, &c.sha, &e.rel_path, &added);
        set(&mut m, o, "medium");
        m.basis.push(why);
    }
    if m.changed_after_agent == Some(true) && m.outcome != "revised" {
        m.basis.push("The committed file isn't byte-for-byte what the agent left, so something else changed in it too.".into());
    }
    Some(m)
}

/// Events from any agent inside this repository between two times (were we listening?).
fn agent_activity(db: &Db, repo: &Repo, from_ms: i64, to_ms: i64) -> usize {
    let conn = db.conn();
    let Ok(mut st) = conn.prepare("SELECT working_dir FROM events WHERE collector_ts BETWEEN ?1 AND ?2 AND working_dir IS NOT NULL LIMIT 5000") else { return 0 };
    let Ok(rows) = st.query_map([from_ms, to_ms], |r| r.get::<_, String>(0)) else { return 0 };
    let root = Path::new(&repo.root);
    rows.filter_map(|r| r.ok()).filter(|wd| cli::same_root(wd, &repo.root) || cli::rel_in_repo(root, Path::new(wd)).is_some()).count()
}

struct CmdFact {
    kind: String,
    command: String,
    succeeded: Option<bool>,
    head_after: Option<String>,
    branch_after: Option<String>,
    observed_at: i64,
    connector: String,
}

fn command_facts(db: &Db, repo: &Repo) -> Result<Vec<CmdFact>> {
    let conn = db.conn();
    let mut st = conn.prepare("SELECT kind, command, succeeded, head_after, branch_after, observed_at, connector, repo_root, repo_identity FROM git_command_facts ORDER BY observed_at")?;
    let rows = st.query_map([], |r| {
        Ok((
            CmdFact { kind: r.get(0)?, command: r.get(1)?, succeeded: r.get(2)?, head_after: r.get(3)?, branch_after: r.get(4)?, observed_at: r.get(5)?, connector: r.get(6)? },
            r.get::<_, String>(7)?,
            r.get::<_, String>(8)?,
        ))
    })?;
    let mut out = vec![];
    for r in rows {
        let (f, root, identity) = r?;
        if identity == repo.identity || cli::same_root(&root, &repo.root) {
            out.push(f);
        }
    }
    Ok(out)
}

fn is_ancestor(root: &Path, a: &str, b: &str) -> bool {
    a == b || cli::run(root, &["merge-base", "--is-ancestor", a, b]).is_ok()
}

/// Builds the full report for one commit. `github` is filled by the caller when enabled.
pub fn inspect(db: &Db, repo: &Repo, rev: &str) -> Result<CommitReport> {
    let root = Path::new(&repo.root);
    let _ = super::capture::import_runtime_edits(db);
    let c = commit_info(root, rev)?;
    let head = cli::try_line(root, &["rev-parse", "HEAD"]);
    let commit_ms = c.commit_time * 1000;
    let parent_ms = c.parents.first().and_then(|p| commit_info(root, p).ok()).map(|p| p.commit_time * 1000);
    let sides = changed_files(root, &c)?;
    let all_edits = edits_for_repo(db, repo, commit_ms + SLACK_MS)?;

    // Pool of committed added lines nobody has explained yet, per file.
    let mut pools: std::collections::HashMap<String, Vec<String>> = sides
        .iter()
        .map(|f| {
            let (mut a, _) = line_changes(f.parent_text.as_deref().unwrap_or(""), f.commit_text.as_deref().unwrap_or(""));
            a.retain(|l| meaningful(l));
            (f.path.clone(), a)
        })
        .collect();

    let mut edits = vec![];
    let mut not_in_commit = vec![];
    for f in &sides {
        let on_file: Vec<&EditRow> = all_edits.iter().filter(|e| e.rel_path == f.path || Some(&e.rel_path) == f.old_path.as_ref()).collect();
        // First pass: every agent line that shipped word for word is explained, whichever edit made it.
        // Only what's left can be matched as "reworded" in the second pass.
        {
            let pool = pools.get_mut(&f.path).expect("pool");
            let (mut c_added, _) = line_changes(f.parent_text.as_deref().unwrap_or(""), f.commit_text.as_deref().unwrap_or(""));
            c_added.retain(|l| meaningful(l));
            for e in &on_file {
                let mut mine = c_added.clone();
                for l in agent_change(e).0 {
                    if take_one(&mut mine, &l) {
                        take_one(pool, &l);
                    }
                }
            }
        }
        // Newest first so the latest agent edit claims the exact-blob match.
        for (i, e) in on_file.iter().rev().enumerate() {
            let pool = pools.get_mut(&f.path).expect("pool");
            if let Some(m) = match_edit(db, root, head.as_deref(), &c, e, Some(f), i == 0, pool) {
                if ["shipped_as_is", "shipped", "revised", "unknown"].contains(&m.outcome.as_str()) {
                    edits.push(m);
                } else {
                    not_in_commit.push(m);
                }
            }
        }
    }
    // Agent edits made since the previous commit to files this commit didn't touch.
    for e in all_edits.iter().filter(|e| parent_ms.map(|p| e.observed_at > p).unwrap_or(true) && !sides.iter().any(|f| f.path == e.rel_path || f.old_path.as_ref() == Some(&e.rel_path))) {
        let mut none = vec![];
        if let Some(m) = match_edit(db, root, head.as_deref(), &c, e, None, false, &mut none) {
            not_in_commit.push(m);
        }
    }
    edits.sort_by(|a, b| a.path.cmp(&b.path).then(a.decision.observed_at.cmp(&b.decision.observed_at)));

    let files: Vec<ShippedFile> = sides
        .iter()
        .map(|f| {
            let (a, r) = line_changes(f.parent_text.as_deref().unwrap_or(""), f.commit_text.as_deref().unwrap_or(""));
            ShippedFile {
                path: f.path.clone(),
                old_path: f.old_path.clone(),
                status: f.status.clone(),
                binary: f.binary,
                added: a.len(),
                removed: r.len(),
                diff: if f.binary { vec![] } else { diff_view(f.parent_text.as_deref().unwrap_or(""), f.commit_text.as_deref().unwrap_or(""), 2) },
                blob: f.blob.clone(),
            }
        })
        .collect();
    let other_sources: Vec<OtherSource> = sides
        .iter()
        .filter_map(|f| {
            let pool = pools.get(&f.path)?;
            (!pool.is_empty()).then(|| OtherSource { path: f.path.clone(), lines: pool.len(), sample: pool.iter().take(12).cloned().collect() })
        })
        .collect();

    let lines_added: usize = sides.iter().map(|f| {
        let (mut a, _) = line_changes(f.parent_text.as_deref().unwrap_or(""), f.commit_text.as_deref().unwrap_or(""));
        a.retain(|l| meaningful(l));
        a.len()
    }).sum();
    let other_lines: usize = other_sources.iter().map(|o| o.lines).sum();
    let reworded: usize = edits.iter().map(|m| m.agent_lines_reworded.len()).sum();
    let summary = Summary {
        lines_added,
        agent_lines_as_proposed: lines_added.saturating_sub(other_lines).saturating_sub(reworded),
        agent_lines_reworded: reworded,
        other_lines,
        files: sides.len(),
        files_with_agent_edits: {
            let mut p: Vec<&str> = edits.iter().filter(|m| m.outcome != "unknown").map(|m| m.path.as_str()).collect();
            p.sort();
            p.dedup();
            p.len()
        },
    };

    let window_from = parent_ms.unwrap_or(commit_ms - 7 * 24 * 3600 * 1000);
    let listening_before = agent_activity(db, repo, window_from, commit_ms + SLACK_MS) > 0;
    let cmds = command_facts(db, repo)?;
    let facts = Facts {
        edit_attribution: edit_fact(&edits, &summary, listening_before),
        commit_authorship: authorship_fact(&c, &cmds, listening_before),
        agent_push: push_fact(root, &c, &cmds, agent_activity(db, repo, commit_ms + 1_000, crate::now_ms()) > 0),
        remote_update: remote_fact(root, &c),
    };
    Ok(CommitReport {
        repo: repo.clone(),
        commit: c,
        files,
        edits,
        not_in_commit,
        other_sources,
        summary,
        facts,
        github: None,
        privacy: "Prompts and agent reasoning are never part of this view or sent anywhere. File text shown here stays on this computer. GitHub only receives the repository name, commit id and branch names, and only when you turn GitHub on for this repository.".into(),
    })
}

fn edit_fact(edits: &[EditMatch], s: &Summary, listening: bool) -> Fact {
    let shipped: Vec<&EditMatch> = edits.iter().filter(|m| ["shipped_as_is", "shipped", "revised"].contains(&m.outcome.as_str())).collect();
    let mut evidence: Vec<String> = shipped.iter().map(|m| format!("{}: {} (decision {}, {} confidence)", m.path, m.outcome_label, m.decision.decision_id, m.confidence)).collect();
    if s.other_lines > 0 {
        evidence.push(format!("{} added line(s) in this commit don't come from any recorded agent edit.", s.other_lines));
    }
    if shipped.is_empty() {
        return if edits.iter().any(|m| m.outcome == "unknown") {
            Fact { status: "unknown", label: "An agent touched files in this commit, but we can't tell what of its work shipped.".into(), evidence: edits.iter().flat_map(|m| m.basis.clone()).collect(), detail: Value::Null }
        } else if listening {
            Fact { status: "not_observed", label: "AgentTracker was listening in this repository and saw no agent edits that ended up in this commit.".into(), evidence, detail: Value::Null }
        } else {
            Fact { status: "unknown", label: "We don't know. AgentTracker has no agent activity recorded for this repository around that time.".into(), evidence, detail: Value::Null }
        };
    }
    let all_high = shipped.iter().all(|m| m.confidence == "high");
    let revised = shipped.iter().filter(|m| m.outcome == "revised").count();
    let label = if revised > 0 {
        format!("Agent edits are in this commit ({} change(s)), and {revised} of them were changed by someone before commit.", shipped.len())
    } else if s.other_lines > 0 {
        format!("Agent edits are in this commit ({} change(s)), mixed with work from someone else.", shipped.len())
    } else {
        format!("Agent edits are in this commit ({} change(s)).", shipped.len())
    };
    Fact { status: if all_high { "confirmed" } else { "likely" }, label, evidence, detail: Value::Null }
}

fn authorship_fact(c: &CommitInfo, cmds: &[CmdFact], listening: bool) -> Fact {
    let detail = serde_json::json!({
        "author": {"name": c.author_name, "email": c.author_email, "time": c.author_time},
        "committer": {"name": c.committer_name, "email": c.committer_email, "time": c.commit_time},
    });
    let mut evidence = vec![
        format!("Git says the author is {} <{}> and the committer is {} <{}>.", c.author_name, c.author_email, c.committer_name, c.committer_email),
        "Git names are whatever the committing program was set up with. Agents usually commit under your name, so this alone doesn't say who ran the commit.".into(),
    ];
    let exact = cmds.iter().find(|f| f.kind == "commit" && f.head_after.as_deref() == Some(c.sha.as_str()));
    if let Some(f) = exact {
        evidence.push(format!("The agent ({}) ran `{}`, and right afterwards the branch pointed at this commit.", f.connector, f.command));
        return Fact { status: "confirmed", label: format!("The agent ran the git commit that created this. Git lists {} as author.", c.author_name), evidence, detail: serde_json::json!({"agent_ran_commit": true, "git": detail}) };
    }
    let near = cmds.iter().find(|f| f.kind == "commit" && f.head_after.is_none() && (f.observed_at - c.commit_time * 1000).abs() <= 10_000);
    if let Some(f) = near {
        evidence.push(format!("The agent ran `{}` within seconds of this commit's time, but we couldn't read which commit it made.", f.command));
        return Fact { status: "likely", label: "The agent probably ran this commit.".into(), evidence, detail: serde_json::json!({"agent_ran_commit": null, "git": detail}) };
    }
    if listening {
        evidence.push("AgentTracker saw agent activity in this repository but no agent git commit that produced this commit.".into());
        Fact { status: "not_observed", label: format!("No agent commit was seen. {} (per Git) or another tool most likely committed it.", c.committer_name), evidence, detail: serde_json::json!({"agent_ran_commit": false, "git": detail}) }
    } else {
        evidence.push("No agent activity was recorded for this repository around that time, so we can't say who ran the commit.".into());
        Fact { status: "unknown", label: format!("Git lists {} as author. Who actually ran the commit is unknown.", c.author_name), evidence, detail: serde_json::json!({"agent_ran_commit": null, "git": detail}) }
    }
}

fn push_fact(root: &Path, c: &CommitInfo, cmds: &[CmdFact], listening_after: bool) -> Fact {
    let candidates: Vec<&CmdFact> = cmds
        .iter()
        .filter(|f| f.kind == "push" && f.observed_at + SLACK_MS >= c.commit_time * 1000)
        .filter(|f| f.head_after.as_deref().map(|h| is_ancestor(root, &c.sha, h)).unwrap_or(true))
        .collect();
    if let Some(f) = candidates.iter().find(|f| f.succeeded == Some(true)).or_else(|| candidates.first()) {
        let base = format!("The agent ({}) ran `{}` after this commit existed (branch {}).", f.connector, f.command, f.branch_after.as_deref().unwrap_or("unknown"));
        return match f.succeeded {
            Some(true) => Fact { status: "confirmed", label: "The agent ran git push for this commit and the command reported success.".into(), evidence: vec![base, "Success comes from the agent's own report of the command. Whether the remote really changed is checked separately below.".into()], detail: Value::Null },
            Some(false) => Fact { status: "no", label: "The agent tried git push, but the command failed.".into(), evidence: vec![base], detail: Value::Null },
            None => Fact { status: "likely", label: "The agent ran git push for this commit. Its result wasn't reported.".into(), evidence: vec![base], detail: Value::Null },
        };
    }
    if listening_after {
        Fact { status: "not_observed", label: "No agent git push was seen for this commit. If it was pushed, a person or another tool did it.".into(), evidence: vec!["AgentTracker saw agent activity in this repository after the commit, but no git push command.".into()], detail: Value::Null }
    } else {
        Fact { status: "unknown", label: "We don't know whether an agent pushed this.".into(), evidence: vec!["No agent activity was recorded for this repository after the commit.".into()], detail: Value::Null }
    }
}

/// Remote-tracking branches on this computer that contain the commit.
pub fn local_remote_branches(root: &Path, sha: &str) -> Vec<String> {
    cli::run(root, &["branch", "-r", "--contains", sha, "--format=%(refname:short)"])
        .map(|s| s.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty() && !l.ends_with("/HEAD") && l.contains('/')).collect())
        .unwrap_or_default()
}

fn remote_fact(root: &Path, c: &CommitInfo) -> Fact {
    let remotes = cli::try_line(root, &["remote"]);
    if remotes.is_none() {
        return Fact { status: "unknown", label: "This repository has no remote, so there's nothing to update.".into(), evidence: vec![], detail: serde_json::json!({"branches": []}) };
    }
    let branches = local_remote_branches(root, &c.sha);
    if branches.is_empty() {
        Fact {
            status: "not_observed",
            label: "No remote branch on this computer includes this commit yet.".into(),
            evidence: vec!["Checked the remote-tracking branches on this computer. They update when you push or fetch, so a push from another computer wouldn't show here until you fetch.".into()],
            detail: serde_json::json!({"branches": []}),
        }
    } else {
        Fact {
            status: "confirmed",
            label: format!("The remote branch {} includes this commit.", branches.join(", ")),
            evidence: vec![format!("Git's remote-tracking branch {} on this computer contains this commit. Git only moves it after a successful push or a fetch, so it reflects the remote as of then.", branches.join(", "))],
            detail: serde_json::json!({"branches": branches, "basis": "local_remote_tracking"}),
        }
    }
}

/// Folds a GitHub answer into the remote fact without hiding disagreements.
pub fn apply_github(report: &mut CommitReport, gh: GithubInfo) {
    let f = &mut report.facts.remote_update;
    if let Some(err) = &gh.error {
        f.evidence.push(format!("GitHub check skipped: {err}"));
    } else if !gh.branches_containing.is_empty() {
        f.status = "confirmed";
        f.label = format!("GitHub confirms branch {} includes this commit.", gh.branches_containing.join(", "));
        f.evidence.push(format!("GitHub's compare API says {} contains this commit.", gh.branches_containing.join(", ")));
    } else if gh.commit_found == Some(false) {
        if f.status == "confirmed" {
            f.status = "unknown";
            f.label = "These disagree: this computer thinks the remote has the commit, but GitHub doesn't know it.".into();
        } else {
            f.status = "no";
            f.label = "GitHub doesn't have this commit.".into();
        }
        f.evidence.push("GitHub answered that it doesn't know this commit id.".into());
    } else if gh.commit_found == Some(true) {
        f.evidence.push(format!("GitHub has this commit, but none of the checked branches ({}) include it.", if gh.branches_checked.is_empty() { "none".to_string() } else { gh.branches_checked.join(", ") }));
    }
    report.github = Some(gh);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_changes_and_similarity() {
        let (a, r) = line_changes("a\nb\nc\n", "a\nB2\nc\nd\n");
        assert_eq!(a, vec!["B2", "d"]);
        assert_eq!(r, vec!["b"]);
        assert!(similarity("return a + b;", "return a + b + 0;") > 0.6);
        assert!(similarity("return a + b;", "fn totally_else() {}") < 0.6);
    }

    #[test]
    fn fragments_widen_to_whole_lines() {
        let before = "fn f() {\n    let x = 10;\n}\n";
        assert_eq!(widen(before, "10", "25", true), Some(("    let x = 10;".into(), "    let x = 25;".into())));
        let after = "fn f() {\n    let x = 25;\n}\n";
        assert_eq!(widen(after, "10", "25", false), Some(("    let x = 10;".into(), "    let x = 25;".into())));
        assert_eq!(widen(before, "", "x", true), None);
        assert_eq!(widen(before, "missing", "x", true), None);
    }
}
