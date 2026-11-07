//! Optional, read-only GitHub lookups. Off unless the user turns it on for a repository.
//! Only the repository name, commit id and branch names are sent; never prompts, reasoning or code.
use serde::Serialize;
use serde_json::Value;
use std::time::Duration;

pub const DEFAULT_API: &str = "https://api.github.com";

#[derive(Debug, Clone, Serialize, Default, PartialEq)]
pub struct PullLink {
    pub number: i64,
    pub title: String,
    pub url: String,
    pub state: String,
    pub merged: bool,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct GithubInfo {
    pub owner: String,
    pub repo: String,
    /// Page on github.com for the commit (built from the slug; valid once GitHub has the commit).
    pub commit_url: String,
    /// Some(true) = GitHub has this commit; Some(false) = it doesn't; None = couldn't ask.
    pub commit_found: Option<bool>,
    pub pulls: Vec<PullLink>,
    /// Branches on GitHub whose tip contains this commit (checked via compare).
    pub branches_containing: Vec<String>,
    pub branches_checked: Vec<String>,
    pub used_token: bool,
    pub error: Option<String>,
}

pub struct GithubClient {
    pub api_base: String,
    pub token: Option<String>,
    agent: ureq::Agent,
}

impl GithubClient {
    pub fn new(api_base: &str, token: Option<String>) -> Self {
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(10)).redirects(2).build();
        Self { api_base: api_base.trim_end_matches('/').to_string(), token: token.filter(|t| !t.trim().is_empty()), agent }
    }

    /// GET returning (status, json). Network failures map to Err with a plain message.
    fn get(&self, path: &str) -> Result<(u16, Value), String> {
        let mut req = self.agent.get(&format!("{}{path}", self.api_base)).set("Accept", "application/vnd.github+json").set("User-Agent", "AgentTracker").set("X-GitHub-Api-Version", "2022-11-28");
        if let Some(t) = &self.token {
            req = req.set("Authorization", &format!("Bearer {t}"));
        }
        match req.call() {
            Ok(r) => {
                let s = r.status();
                Ok((s, r.into_json::<Value>().unwrap_or(Value::Null)))
            }
            Err(ureq::Error::Status(code, r)) => Ok((code, r.into_json::<Value>().unwrap_or(Value::Null))),
            Err(e) => Err(format!("Couldn't reach GitHub: {e}")),
        }
    }

    pub fn lookup(&self, owner: &str, repo: &str, sha: &str, branches: &[String]) -> GithubInfo {
        let mut info = GithubInfo {
            owner: owner.into(),
            repo: repo.into(),
            commit_url: format!("https://github.com/{owner}/{repo}/commit/{sha}"),
            used_token: self.token.is_some(),
            ..Default::default()
        };
        let enc = |s: &str| s.replace('%', "%25").replace(' ', "%20").replace('#', "%23").replace('?', "%3F");
        match self.get(&format!("/repos/{owner}/{repo}/commits/{sha}")) {
            Ok((200, v)) => {
                info.commit_found = Some(true);
                if let Some(u) = v.get("html_url").and_then(Value::as_str) {
                    info.commit_url = u.to_string();
                }
            }
            Ok((404 | 422, _)) => info.commit_found = Some(false),
            Ok((401, _)) => {
                info.error = Some("GitHub didn't accept the access token. Check it in Code changes, GitHub access.".into());
                return info;
            }
            Ok((403, _)) => {
                info.error = Some("GitHub refused the request (rate limit or missing permission).".into());
                return info;
            }
            Ok((code, _)) => {
                info.error = Some(format!("GitHub answered with status {code}."));
                return info;
            }
            Err(e) => {
                info.error = Some(e);
                return info;
            }
        }
        if info.commit_found != Some(true) {
            return info;
        }
        if let Ok((200, Value::Array(a))) = self.get(&format!("/repos/{owner}/{repo}/commits/{sha}/pulls")) {
            info.pulls = a
                .iter()
                .map(|p| PullLink {
                    number: p.get("number").and_then(Value::as_i64).unwrap_or(0),
                    title: p.get("title").and_then(Value::as_str).unwrap_or("").to_string(),
                    url: p.get("html_url").and_then(Value::as_str).unwrap_or("").to_string(),
                    state: p.get("state").and_then(Value::as_str).unwrap_or("").to_string(),
                    merged: p.get("merged_at").map(|m| !m.is_null()).unwrap_or(false),
                })
                .collect();
        }
        for b in branches.iter().take(4) {
            info.branches_checked.push(b.clone());
            if let Ok((200, v)) = self.get(&format!("/repos/{owner}/{repo}/compare/{}...{sha}", enc(b))) {
                if matches!(v.get("status").and_then(Value::as_str), Some("identical") | Some("behind")) {
                    info.branches_containing.push(b.clone());
                }
            }
        }
        info
    }
}
