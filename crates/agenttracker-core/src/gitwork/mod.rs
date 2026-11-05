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
        let rows = st.query_map([], row_to_repo)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn get(&self, id: &str) -> Result<Repo> {
        let conn = self.db.conn();
        conn.query_row(&format!("SELECT {COLS} FROM git_repos WHERE id=?1"), [id], row_to_repo)
            .optional()?
            .ok_or_else(|| CoreError::NotFound(format!("repository {id}")))
    }

    /// Forgets the repository in AgentTracker only; the folder itself is untouched.
    pub fn remove(&self, id: &str) -> Result<()> {
        let n = self.db.conn().execute("DELETE FROM git_repos WHERE id=?1", [id])?;
        if n == 0 {
            return Err(CoreError::NotFound(format!("repository {id}")));
        }
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
