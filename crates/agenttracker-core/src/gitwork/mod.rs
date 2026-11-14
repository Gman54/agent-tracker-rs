//! Local Git tracking: which agent edits ended up in which commit, and an optional
//! read-only GitHub lookup for links. Everything is read-only toward the repository.
pub mod capture;
pub mod cli;
pub mod github;
pub mod matching;

use crate::db::Db;
use crate::{CoreError, Result};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use std::path::Path;

/// Settings key holding the GitHub token. Never sent back to the UI.
pub const TOKEN_KEY: &str = "github_token";
/// Settings key overriding the GitHub API address (tests use a local mock).
pub const API_BASE_KEY: &str = "github_api_base";

#[derive(Debug, Clone, Serialize)]
pub struct Repo {
    pub id: String,
    pub root: String,
    pub identity: String,
    pub remote_url: Option<String>,
    pub github_owner: Option<String>,
    pub github_repo: Option<String>,
    pub github_enabled: bool,
    pub added_at: i64,
    /// When "Check the remote now" last finished successfully (ms), if ever.
    pub last_fetched_at: Option<i64>,
}

/// Result of "Check the remote now".
#[derive(Debug, Clone, Serialize)]
pub struct FetchOutcome {
    /// True when any remote-tracking branch moved, appeared or disappeared.
    pub changed: bool,
    pub message: String,
    pub fetched_at: i64,
    pub repo: Repo,
}

/// How many projects "Check all projects" checks at the same time.
pub const FETCH_ALL_WORKERS: usize = 4;

/// One project's line in "Check all projects".
#[derive(Debug, Clone, Serialize)]
pub struct ProjectCheck {
    pub repo_id: String,
    pub name: String,
    pub root: String,
    /// "changed", "up_to_date", "skipped" (no remote) or "failed".
    pub status: &'static str,
    pub message: String,
    pub last_fetched_at: Option<i64>,
}

/// Result of "Check all projects".
#[derive(Debug, Clone, Serialize)]
pub struct FetchAllOutcome {
    pub results: Vec<ProjectCheck>,
    pub checked: usize,
    pub changed: usize,
    pub up_to_date: usize,
    pub failed: usize,
    pub skipped: usize,
    pub summary: String,
}

impl FetchAllOutcome {
    fn from_results(results: Vec<ProjectCheck>) -> Self {
        let n = |s: &str| results.iter().filter(|r| r.status == s).count();
        let (changed, up_to_date, failed, skipped) = (n("changed"), n("up_to_date"), n("failed"), n("skipped"));
        let checked = changed + up_to_date;
        let summary = fetch_all_summary(results.len(), checked, changed, up_to_date, failed, skipped);
        Self { results, checked, changed, up_to_date, failed, skipped, summary }
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// A short, friendly line such as "3 checked: 1 had new changes, 2 up to date. 1 couldn't be checked."
pub fn fetch_all_summary(total: usize, checked: usize, changed: usize, up_to_date: usize, failed: usize, skipped: usize) -> String {
    if total == 0 {
        return "There are no projects to check yet.".into();
    }
    let mut out = if checked > 0 {
        let mut parts = vec![];
        if changed > 0 {
            parts.push(format!("{changed} had new changes"));
        }
        if up_to_date > 0 {
            parts.push(format!("{up_to_date} up to date"));
        }
        format!("{} checked: {}.", checked, parts.join(", "))
    } else if failed > 0 {
        "None could be checked.".to_string()
    } else {
        "Nothing needed checking.".to_string()
    };
    if failed > 0 {
        out.push_str(&format!(" {} couldn't be checked.", failed));
    }
    if skipped > 0 {
        out.push_str(&format!(" {} only on this computer, so skipped.", plural(skipped, "project is", "projects are")));
    }
    out
}

/// Last part of a project folder, for display.
pub fn repo_display_name(root: &str) -> String {
    let t = root.trim_end_matches(['/', '\\']);
    t.rsplit(['/', '\\']).next().filter(|s| !s.is_empty()).unwrap_or(t).to_string()
}

/// The remote-tracking branches and the commits they point at, used to tell whether a check
/// brought in anything new. `origin/HEAD` is left out: newer Git (2.48+, as shipped with Git for
/// Windows) creates it on the first fetch, which isn't a change on the remote.
pub fn remote_branch_snapshot(root: &Path) -> Vec<String> {
    cli::run(root, &["for-each-ref", "--format=%(refname) %(objectname)", "refs/remotes"])
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.split(' ').next().unwrap_or("").ends_with("/HEAD"))
        .map(str::to_string)
        .collect()
}

fn fetch_key(id: &str) -> String {
    format!("git_last_fetch:{id}")
}

fn row_to_repo(r: &rusqlite::Row) -> rusqlite::Result<Repo> {
    Ok(Repo {
        id: r.get(0)?,
        root: r.get(1)?,
        identity: r.get(2)?,
        remote_url: r.get(3)?,
        github_owner: r.get(4)?,
        github_repo: r.get(5)?,
        github_enabled: r.get::<_, i64>(6)? != 0,
        added_at: r.get(7)?,
        last_fetched_at: None,
    })
}

const COLS: &str = "id, root, identity, remote_url, github_owner, github_repo, github_enabled, added_at";

pub struct RepoStore<'a> {
    db: &'a Db,
}

impl<'a> RepoStore<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self { db }
    }

    /// Adds a folder that is (or is inside) a Git repository. Adding the same repo twice
    /// refreshes its remote details and returns the existing entry.
    pub fn add(&self, path: &str) -> Result<Repo> {
        let p = Path::new(path.trim());
        if path.trim().is_empty() || !p.is_dir() {
            return Err(CoreError::Invalid("That folder doesn't exist.".into()));
        }
        let top = cli::run(p, &["rev-parse", "--show-toplevel"])
            .map_err(|_| CoreError::Invalid("That folder isn't part of a Git repository.".into()))?;
        let top = top.trim();
        // git prints forward slashes on Windows; keep the OS form for display and matching.
        let root_path = std::fs::canonicalize(top).unwrap_or_else(|_| Path::new(top).to_path_buf());
        let root = strip_verbatim(&root_path.to_string_lossy());
        let (identity, remote) = cli::repo_identity(Path::new(&root));
        let slug = remote.as_deref().and_then(cli::github_slug);
        let existing = self.find_by_root(&root)?;
        let conn = self.db.conn();
        let id = match existing {
            Some(r) => {
                conn.execute(
                    "UPDATE git_repos SET identity=?2, remote_url=?3, github_owner=?4, github_repo=?5 WHERE id=?1",
                    params![r.id, identity, remote, slug.as_ref().map(|s| &s.0), slug.as_ref().map(|s| &s.1)],
                )?;
                r.id
            }
            None => {
                let id = uuid::Uuid::new_v4().to_string();
                conn.execute(
                    "INSERT INTO git_repos (id, root, identity, remote_url, github_owner, github_repo, github_enabled, added_at) VALUES (?1,?2,?3,?4,?5,?6,0,?7)",
                    params![id, root, identity, remote, slug.as_ref().map(|s| &s.0), slug.as_ref().map(|s| &s.1), crate::now_ms()],
                )?;
                id
            }
        };
        drop(conn);
        self.get(&id)
    }

    fn find_by_root(&self, root: &str) -> Result<Option<Repo>> {
        Ok(self.list()?.into_iter().find(|r| cli::same_root(&r.root, root)))
    }

    pub fn list(&self) -> Result<Vec<Repo>> {
        let conn = self.db.conn();
        let mut st = conn.prepare(&format!("SELECT {COLS} FROM git_repos ORDER BY added_at"))?;
        let mut rows = st.query_map([], row_to_repo)?.collect::<rusqlite::Result<Vec<_>>>()?;
        drop(st);
        drop(conn);
        for r in &mut rows {
            self.fill_fetched(r);
        }
        Ok(rows)
    }

    fn fill_fetched(&self, r: &mut Repo) {
        r.last_fetched_at = self.db.get_setting(&fetch_key(&r.id)).ok().flatten().and_then(|v| v.parse().ok());
    }

    pub fn get(&self, id: &str) -> Result<Repo> {
        let found = self.db.conn().query_row(&format!("SELECT {COLS} FROM git_repos WHERE id=?1"), [id], row_to_repo).optional()?;
        let mut r = found.ok_or_else(|| CoreError::NotFound(format!("repository {id}")))?;
        self.fill_fetched(&mut r);
        Ok(r)
    }

    /// "Check the remote now": runs `git fetch --all --prune` in the project. Only the
    /// remote-tracking branches change; the user's files and branches are left alone.
    pub fn fetch(&self, id: &str) -> Result<FetchOutcome> {
        self.fetch_with_timeout(id, cli::FETCH_TIMEOUT)
    }

    pub fn fetch_with_timeout(&self, id: &str, timeout: std::time::Duration) -> Result<FetchOutcome> {
        let repo = self.get(id)?;
        let root = Path::new(&repo.root);
        if !root.is_dir() {
            return Err(CoreError::Invalid("This project's folder can't be found any more. It may have been moved or deleted.".into()));
        }
        if cli::try_line(root, &["remote"]).is_none() {
            return Err(CoreError::Invalid("This project has no remote to check. It only lives on this computer.".into()));
        }
        let before = remote_branch_snapshot(root);
        cli::fetch(root, timeout)?;
        let changed = remote_branch_snapshot(root) != before;
        let fetched_at = crate::now_ms();
        self.db.set_setting(&fetch_key(id), &fetched_at.to_string())?;
        let message = if changed { "New changes came in from the remote." } else { "Already up to date with the remote." };
        Ok(FetchOutcome { changed, message: message.into(), fetched_at, repo: self.get(id)? })
    }

    /// "Check all projects": checks every project's remote. Projects without a remote are
    /// skipped, a failure in one project never stops the others, and up to
    /// [`FETCH_ALL_WORKERS`] projects are checked at the same time so one slow remote doesn't
    /// hold up the rest. Each project keeps its own time limit.
    pub fn fetch_all(&self) -> Result<FetchAllOutcome> {
        self.fetch_all_with_timeout(cli::FETCH_TIMEOUT)
    }

    pub fn fetch_all_with_timeout(&self, timeout: std::time::Duration) -> Result<FetchAllOutcome> {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Mutex;
        let repos = self.list()?;
        let slots: Vec<Mutex<Option<ProjectCheck>>> = repos.iter().map(|_| Mutex::new(None)).collect();
        let next = AtomicUsize::new(0);
        let workers = FETCH_ALL_WORKERS.min(repos.len()).max(1);
        std::thread::scope(|s| {
            for _ in 0..workers {
                s.spawn(|| loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    let Some(repo) = repos.get(i) else { break };
                    let check = self.check_one(repo, timeout);
                    *slots[i].lock().unwrap_or_else(|p| p.into_inner()) = Some(check);
                });
            }
        });
        let results: Vec<ProjectCheck> = slots.into_iter().map(|m| m.into_inner().unwrap_or_else(|p| p.into_inner()).expect("every project checked")).collect();
        Ok(FetchAllOutcome::from_results(results))
    }

    fn check_one(&self, repo: &Repo, timeout: std::time::Duration) -> ProjectCheck {
        let name = repo_display_name(&repo.root);
        let mk = |status: &'static str, message: String, last: Option<i64>| ProjectCheck { repo_id: repo.id.clone(), name: name.clone(), root: repo.root.clone(), status, message, last_fetched_at: last };
        let root = Path::new(&repo.root);
        if root.is_dir() && cli::try_line(root, &["remote"]).is_none() {
            return mk("skipped", "Only on this computer, so there's nothing to check.".into(), repo.last_fetched_at);
        }
        match self.fetch_with_timeout(&repo.id, timeout) {
            Ok(o) => mk(if o.changed { "changed" } else { "up_to_date" }, o.message, Some(o.fetched_at)),
            Err(e) => mk("failed", e.to_string(), repo.last_fetched_at),
        }
    }

    /// Forgets the repository in Houndbit only; the folder itself is untouched.
    pub fn remove(&self, id: &str) -> Result<()> {
        let n = self.db.conn().execute("DELETE FROM git_repos WHERE id=?1", [id])?;
        if n == 0 {
            return Err(CoreError::NotFound(format!("repository {id}")));
        }
        self.db.conn().execute("DELETE FROM settings WHERE key=?1", [fetch_key(id)])?;
        Ok(())
    }

    pub fn set_github(&self, id: &str, enabled: bool) -> Result<Repo> {
        let r = self.get(id)?;
        if enabled && r.github_owner.is_none() {
            return Err(CoreError::Invalid("This repository doesn't have a GitHub remote named origin.".into()));
        }
        self.db.conn().execute("UPDATE git_repos SET github_enabled=?2 WHERE id=?1", params![id, enabled as i64])?;
        self.get(id)
    }
}

fn strip_verbatim(s: &str) -> String {
    s.strip_prefix(r"\\?\").unwrap_or(s).to_string()
}

pub fn has_token(db: &Db) -> bool {
    db.get_setting(TOKEN_KEY).ok().flatten().map(|t| !t.trim().is_empty()).unwrap_or(false)
}

/// Saves (or clears, when empty) the GitHub token.
pub fn set_token(db: &Db, token: &str) -> Result<()> {
    db.set_setting(TOKEN_KEY, token.trim())
}

/// Builds a read-only GitHub client from saved settings.
pub fn github_client(db: &Db) -> github::GithubClient {
    let base = db.get_setting(API_BASE_KEY).ok().flatten().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| github::DEFAULT_API.to_string());
    let token = db.get_setting(TOKEN_KEY).ok().flatten().filter(|s| !s.trim().is_empty());
    github::GithubClient::new(&base, token)
}

/// Inspects a commit and, if the user turned GitHub on for this repository, adds links.
pub fn inspect_with_github(db: &Db, repo: &Repo, rev: &str) -> Result<matching::CommitReport> {
    let mut report = matching::inspect(db, repo, rev)?;
    if repo.github_enabled {
        if let (Some(o), Some(n)) = (&repo.github_owner, &repo.github_repo) {
            // Only branches on the origin remote are GitHub branches; drop the "origin/" prefix.
            let mut branches: Vec<String> = matching::local_remote_branches(Path::new(&repo.root), &report.commit.sha)
                .into_iter()
                .filter_map(|b| b.strip_prefix("origin/").map(str::to_string))
                .collect();
            if branches.is_empty() {
                if let Some(b) = cli::try_line(Path::new(&repo.root), &["rev-parse", "--abbrev-ref", "HEAD"]).filter(|b| b != "HEAD") {
                    branches.push(b);
                }
            }
            let info = github_client(db).lookup(o, n, &report.commit.sha, &branches);
            matching::apply_github(&mut report, info);
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetch_all_summary_reads_naturally() {
        assert_eq!(fetch_all_summary(4, 3, 1, 2, 1, 0), "3 checked: 1 had new changes, 2 up to date. 1 couldn't be checked.");
        assert_eq!(fetch_all_summary(2, 2, 0, 2, 0, 0), "2 checked: 2 up to date.");
        assert_eq!(fetch_all_summary(2, 0, 0, 0, 1, 1), "None could be checked. 1 couldn't be checked. 1 project is only on this computer, so skipped.");
        assert_eq!(fetch_all_summary(2, 0, 0, 0, 0, 2), "Nothing needed checking. 2 projects are only on this computer, so skipped.");
        assert_eq!(fetch_all_summary(0, 0, 0, 0, 0, 0), "There are no projects to check yet.");
        assert_eq!(repo_display_name(r"C:\Users\Greg\proj\"), "proj");
        assert_eq!(repo_display_name("/home/g/site"), "site");
    }
}
