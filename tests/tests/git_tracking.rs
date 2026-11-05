//! Git tracking acceptance tests. Real `git` repositories in temp folders; the "agent" is fake hook
//! deliveries in Claude Code's format plus file writes done by the test. The GitHub part talks to a
//! local mock server, never to github.com.
use agenttracker_acceptance::{claude_envelope, Fixture};
use agenttracker_core::gitwork::{self, matching::CommitReport, RepoStore};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=Greg", "-c", "user.email=greg@example.com", "-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("run git");
    assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

struct Proj {
    fx: Fixture,
    root: PathBuf,
    ts: std::cell::Cell<i64>,
}

const BASE: &str = "fn main() {\n    let speed = 10;\n    println!(\"speed {speed}\");\n}\n";

impl Proj {
    /// A repo with one human commit already pushed to a bare "origin".
    fn new() -> Self {
        let fx = Fixture::new();
        let remote = fx.dir.path().join("remote.git");
        let root = fx.dir.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        git(fx.dir.path(), &["init", "-q", "--bare", "-b", "main", remote.to_str().unwrap()]);
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["remote", "add", "origin", remote.to_str().unwrap()]);
        std::fs::write(root.join("main.rs"), BASE).unwrap();
        std::fs::write(root.join("notes.txt"), "todo\n").unwrap();
        git(&root, &["add", "main.rs", "notes.txt"]);
        git(&root, &["commit", "-q", "-m", "start"]);
        git(&root, &["push", "-q", "-u", "origin", "main"]);
        // Commit times are whole seconds; keep later commits clearly after the first one.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        Proj { fx, root, ts: std::cell::Cell::new(1_000) }
    }

    fn next_ts(&self) -> i64 {
        self.ts.set(self.ts.get() + 1_000);
        self.ts.get()
    }

    fn path(&self, rel: &str) -> String {
        self.root.join(rel).to_string_lossy().into_owned()
    }

    fn cwd(&self) -> String {
        self.root.to_string_lossy().into_owned()
    }

    /// A fake Claude Code Edit: PreToolUse, the edit on disk, then PostToolUse.
    fn agent_edit(&self, id: &str, rel: &str, old: &str, new: &str) {
        let file = self.path(rel);
        let input = json!({"file_path": file, "old_string": old, "new_string": new});
        let pre = json!({"session_id": "s1", "cwd": self.cwd(), "tool_name": "Edit", "tool_use_id": id, "tool_input": input});
        self.fx.deliver(claude_envelope("PreToolUse", self.next_ts(), pre));
        let before = std::fs::read_to_string(&file).unwrap();
        assert!(before.contains(old), "test setup: {old:?} not in {rel}");
        std::fs::write(&file, before.replacen(old, new, 1)).unwrap();
        let post = json!({"session_id": "s1", "cwd": self.cwd(), "tool_name": "Edit", "tool_use_id": id, "tool_input": input,
            "tool_response": {"filePath": file, "oldString": old, "newString": new, "userModified": false}});
        self.fx.deliver(claude_envelope("PostToolUse", self.next_ts(), post));
    }

    /// A fake Claude Code Bash call whose effects the test has already performed.
    fn agent_bash(&self, id: &str, command: &str, failed: bool, stderr: &str) {
        let input = json!({"command": command});
        self.fx.deliver(claude_envelope("PreToolUse", self.next_ts(), json!({"session_id": "s1", "cwd": self.cwd(), "tool_name": "Bash", "tool_use_id": id, "tool_input": input})));
        let mut post = json!({"session_id": "s1", "cwd": self.cwd(), "tool_name": "Bash", "tool_use_id": id, "tool_input": input});
        if failed {
            post["error"] = json!(stderr);
        } else {
            post["tool_response"] = json!({"stdout": "", "stderr": stderr, "interrupted": false});
        }
        self.fx.deliver(claude_envelope(if failed { "PostToolUseFailure" } else { "PostToolUse" }, self.next_ts(), post));
    }

    /// Any later agent activity in the repo (shows AgentTracker was listening).
    fn agent_reads(&self, id: &str) {
        let p = json!({"session_id": "s1", "cwd": self.cwd(), "tool_name": "Read", "tool_use_id": id, "tool_input": {"file_path": self.path("main.rs")}, "tool_response": {}});
        self.fx.deliver(claude_envelope("PostToolUse", self.next_ts(), p));
    }

    fn repo(&self) -> gitwork::Repo {
        RepoStore::new(&self.fx.db).add(&self.cwd()).expect("add repo")
    }

    fn head(&self) -> String {
        git(&self.root, &["rev-parse", "HEAD"])
    }

    fn report(&self) -> CommitReport {
        let r = self.repo();
        gitwork::inspect_with_github(&self.fx.db, &r, &self.head()).expect("inspect")
    }
}

fn outcomes(r: &CommitReport) -> Vec<(String, String)> {
    r.edits.iter().map(|m| (m.path.clone(), m.outcome.clone())).collect()
}

// ---- the three required cases ---------------------------------------------------------------------

#[test]
fn case1_agent_edits_and_pushes() {
    let p = Proj::new();
    p.agent_edit("tu-edit-1", "main.rs", "let speed = 10;", "let speed = 25;");
    git(&p.root, &["add", "main.rs"]);
    git(&p.root, &["commit", "-q", "-m", "faster"]);
    git(&p.root, &["push", "-q", "origin", "main"]);
    p.agent_bash("tu-bash-1", "git add main.rs && git commit -m faster && git push origin main", false, "To remote.git\n   abc..def  main -> main");

    let r = p.report();
    assert_eq!(outcomes(&r), vec![("main.rs".into(), "shipped_as_is".into())]);
    let m = &r.edits[0];
    assert_eq!(m.decision.decision_id, "tu-edit-1");
    assert_eq!(m.confidence, "high");
    assert!(m.basis.iter().any(|b| b.contains("blob id")), "{:?}", m.basis);
    assert_eq!(m.before_evidence["source"], "pre_hook_snapshot");
    assert_eq!(r.facts.edit_attribution.status, "confirmed");
    assert_eq!(r.facts.commit_authorship.status, "confirmed", "{:?}", r.facts.commit_authorship);
    assert_eq!(r.facts.agent_push.status, "confirmed", "{:?}", r.facts.agent_push);
    assert_eq!(r.facts.remote_update.status, "confirmed", "{:?}", r.facts.remote_update);
    assert!(r.other_sources.is_empty());
    assert_eq!(r.repo.identity, r.repo.identity.trim());
}

#[test]
fn case2_agent_edits_human_pushes() {
    let p = Proj::new();
    p.agent_edit("tu-edit-2", "main.rs", "let speed = 10;", "let speed = 40;");
    // A person commits and pushes from their own terminal; no agent command is involved.
    git(&p.root, &["add", "main.rs"]);
    git(&p.root, &["commit", "-q", "-m", "tweak"]);
    git(&p.root, &["push", "-q", "origin", "main"]);

    // Right away, with no agent activity after the commit, push is honestly unknown.
    let r = p.report();
    assert_eq!(outcomes(&r), vec![("main.rs".into(), "shipped_as_is".into())]);
    assert_eq!(r.facts.edit_attribution.status, "confirmed");
    assert_eq!(r.facts.commit_authorship.status, "not_observed", "{:?}", r.facts.commit_authorship);
    assert_eq!(r.facts.remote_update.status, "confirmed");
    assert_ne!(r.facts.agent_push.status, "confirmed");
    assert_ne!(r.facts.agent_push.status, "likely");

    // Once the agent is seen working in the repo later without pushing, it's "not observed".
    std::thread::sleep(std::time::Duration::from_millis(2100));
    p.agent_reads("tu-read-2");
    let r = p.report();
    assert_eq!(r.facts.agent_push.status, "not_observed", "{:?}", r.facts.agent_push);
    assert_eq!(r.facts.remote_update.status, "confirmed");
    assert_eq!(r.facts.commit_authorship.detail["git"]["author"]["name"], "Greg");
}

#[test]
fn case3_human_revises_agent_edit_before_commit() {
    let p = Proj::new();
    p.agent_edit("tu-edit-3", "main.rs", "    println!(\"speed {speed}\");", "    println!(\"current speed is {speed} km/h\");");
    // Greg rewords the agent's line before committing.
    let f = p.root.join("main.rs");
    let t = std::fs::read_to_string(&f).unwrap().replace("current speed is {speed} km/h", "current speed: {speed} km/h");
    std::fs::write(&f, t).unwrap();
    git(&p.root, &["add", "main.rs"]);
    git(&p.root, &["commit", "-q", "-m", "message"]);

    let r = p.report();
    assert_eq!(outcomes(&r), vec![("main.rs".into(), "revised".into())]);
    let m = &r.edits[0];
    assert_eq!(m.changed_after_agent, Some(true));
    // What the agent proposed ...
    assert!(m.proposed.iter().any(|l| l.kind == "add" && l.text.contains("current speed is {speed} km/h")), "{:?}", m.proposed);
    // ... versus what actually shipped.
    assert_eq!(m.agent_lines_reworded.len(), 1);
    assert!(m.agent_lines_reworded[0].proposed.contains("current speed is"));
    assert!(m.agent_lines_reworded[0].shipped.contains("current speed: {speed}"));
    assert!(m.agent_lines_shipped.is_empty());
    let shipped = &r.files[0].diff;
    assert!(shipped.iter().any(|l| l.kind == "add" && l.text.contains("current speed: {speed}")));
    assert!(!shipped.iter().any(|l| l.text.contains("current speed is")));
    assert!(m.basis.iter().any(|b| b.contains("reworded")), "{:?}", m.basis);
    assert_eq!(r.summary.agent_lines_reworded, 1);
    assert!(r.facts.edit_attribution.label.contains("changed by someone"), "{}", r.facts.edit_attribution.label);
    assert_eq!(r.facts.agent_push.status, "unknown");
    assert_eq!(r.facts.remote_update.status, "not_observed");
}

// ---- staging, intervening edits, multiple sources, missing evidence ---------------------------------

#[test]
fn partial_staging_across_files_and_inside_a_file() {
    let p = Proj::new();
    p.agent_edit("tu-a", "notes.txt", "todo", "todo\nbuy fuel");
    p.agent_edit("tu-b", "main.rs", "let speed = 10;", "let speed = 10;\n    let shields = 3;");
    p.agent_edit("tu-c", "main.rs", "}\n", "    println!(\"shields {shields}\");\n}\n");
    // Stage only the first main.rs change (like `git add -p`) and leave notes.txt unstaged.
    let staged = BASE.replace("let speed = 10;", "let speed = 10;\n    let shields = 3;");
    let blob = {
        std::fs::write(p.fx.dir.path().join("staged.rs"), &staged).unwrap();
        git(&p.root, &["hash-object", "-w", p.fx.dir.path().join("staged.rs").to_str().unwrap()])
    };
    git(&p.root, &["update-index", "--cacheinfo", &format!("100644,{blob},main.rs")]);
    git(&p.root, &["commit", "-q", "-m", "shields"]);

    let r = p.report();
    let by_dec = |d: &str| r.edits.iter().chain(r.not_in_commit.iter()).find(|m| m.decision.decision_id == d).cloned().unwrap_or_else(|| panic!("no match for {d}: {:?}", outcomes(&r)));
    assert_eq!(by_dec("tu-b").outcome, "shipped");
    let c = by_dec("tu-c");
    assert_eq!(c.outcome, "not_staged", "{:?}", c.basis);
    let a = by_dec("tu-a");
    assert_eq!(a.outcome, "not_staged");
    assert!(r.not_in_commit.iter().any(|m| m.decision.decision_id == "tu-a"));
    assert!(r.other_sources.is_empty(), "{:?}", r.other_sources);
}

#[test]
fn intervening_human_edit_makes_a_mixed_commit() {
    let p = Proj::new();
    p.agent_edit("tu-m", "main.rs", "let speed = 10;", "let speed = 12;");
    // A person adds their own line to the same file, and edits another file, before committing.
    let f = p.root.join("main.rs");
    std::fs::write(&f, std::fs::read_to_string(&f).unwrap().replace("fn main() {\n", "fn main() {\n    // tuned by hand\n")).unwrap();
    std::fs::write(p.root.join("notes.txt"), "todo\nask about warp\n").unwrap();
    git(&p.root, &["add", "main.rs", "notes.txt"]);
    git(&p.root, &["commit", "-q", "-m", "mixed"]);

    let r = p.report();
    assert_eq!(outcomes(&r), vec![("main.rs".into(), "shipped".into())]);
    let m = &r.edits[0];
    assert_eq!(m.changed_after_agent, Some(true));
    assert!(m.basis.iter().any(|b| b.contains("something else changed")), "{:?}", m.basis);
    let others: Vec<&str> = r.other_sources.iter().map(|o| o.path.as_str()).collect();
    assert_eq!(others, vec!["main.rs", "notes.txt"]);
    assert!(r.other_sources[0].sample.iter().any(|l| l.contains("tuned by hand")));
    assert_eq!(r.facts.edit_attribution.status, "confirmed");
    assert!(r.facts.edit_attribution.label.contains("someone else"));
}

#[test]
fn missing_evidence_stays_unknown() {
    let p = Proj::new();
    std::fs::write(p.root.join("notes.txt"), "todo\nwritten with no agent watching\n").unwrap();
    git(&p.root, &["add", "notes.txt"]);
    git(&p.root, &["commit", "-q", "-m", "by hand"]);
    let r = p.report();
    assert!(r.edits.is_empty());
    assert_eq!(r.facts.edit_attribution.status, "unknown");
    assert_eq!(r.facts.commit_authorship.status, "unknown");
    assert_eq!(r.facts.agent_push.status, "unknown");
    assert_eq!(r.other_sources.len(), 1);
}

#[test]
fn edit_without_before_snapshot_still_matches_with_lower_basis() {
    let p = Proj::new();
    // Only a PostToolUse arrives (e.g. the PreToolUse hook wasn't installed).
    let file = p.path("main.rs");
    std::fs::write(&file, BASE.replace("10", "11")).unwrap();
    let input = json!({"file_path": file, "old_string": "let speed = 10;", "new_string": "let speed = 11;"});
    p.fx.deliver(claude_envelope("PostToolUse", 5, json!({"session_id": "s1", "cwd": p.cwd(), "tool_name": "Edit", "tool_use_id": "tu-late", "tool_input": input, "tool_response": {}})));
    git(&p.root, &["commit", "-q", "-am", "eleven"]);
    let r = p.report();
    assert_eq!(r.edits[0].before_evidence["source"], "none");
    assert_eq!(r.edits[0].outcome, "shipped_as_is");
}

#[test]
fn agent_push_that_failed_is_recorded_as_failed() {
    let p = Proj::new();
    p.agent_edit("tu-f", "main.rs", "let speed = 10;", "let speed = 99;");
    git(&p.root, &["commit", "-q", "-am", "ninety-nine"]);
    p.agent_bash("tu-push-f", "git push origin main", true, "! [rejected] main -> main (fetch first)\nerror: failed to push some refs");
    let r = p.report();
    assert_eq!(r.facts.agent_push.status, "no", "{:?}", r.facts.agent_push);
    assert_eq!(r.facts.remote_update.status, "not_observed");
    assert_eq!(r.facts.commit_authorship.status, "not_observed");
}

#[test]
fn echo_git_push_is_not_a_push() {
    let p = Proj::new();
    p.agent_edit("tu-e", "main.rs", "let speed = 10;", "let speed = 5;");
    git(&p.root, &["commit", "-q", "-am", "five"]);
    p.agent_bash("tu-echo", "echo git push origin main", false, "");
    let r = p.report();
    assert_ne!(r.facts.agent_push.status, "confirmed");
}

// ---- GitHub (mock server) ---------------------------------------------------------------------------

/// A tiny HTTP server that answers like GitHub's REST API and records each request line + auth header.
fn github_mock(sha: String) -> (String, Arc<Mutex<Vec<String>>>) {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = format!("http://{}", l.local_addr().unwrap());
    let log = Arc::new(Mutex::new(vec![]));
    let log2 = log.clone();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            let mut auth = String::new();
            loop {
                let mut h = String::new();
                if r.read_line(&mut h).unwrap() == 0 || h == "\r\n" {
                    break;
                }
                if h.to_ascii_lowercase().starts_with("authorization:") {
                    auth = h.trim().to_string();
                }
            }
            let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
            log2.lock().unwrap().push(format!("{path} | {auth}"));
            let (code, body): (u16, Value) = if path == format!("/repos/acme/widgets/commits/{sha}") {
                (200, json!({"sha": sha, "html_url": format!("https://github.com/acme/widgets/commit/{sha}")}))
            } else if path == format!("/repos/acme/widgets/commits/{sha}/pulls") {
                (200, json!([{"number": 7, "title": "Faster ships", "html_url": "https://github.com/acme/widgets/pull/7", "state": "open", "merged_at": null}]))
            } else if path == format!("/repos/acme/widgets/compare/main...{sha}") {
                (200, json!({"status": "identical"}))
            } else {
                (404, json!({"message": "Not Found"}))
            };
            let b = body.to_string();
            let mut s = s;
            let _ = write!(s, "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}", b.len());
        }
    });
    (addr, log)
}

#[test]
fn github_links_need_opt_in_and_send_no_prompts() {
    let p = Proj::new();
    p.fx.deliver(claude_envelope("UserPromptSubmit", 2, json!({"session_id": "s1", "cwd": p.cwd(), "prompt": "SECRET-PROMPT-TEXT make it faster"})));
    p.agent_edit("tu-g", "main.rs", "let speed = 10;", "let speed = 30;");
    git(&p.root, &["commit", "-q", "-am", "thirty"]);
    git(&p.root, &["push", "-q", "origin", "main"]);
    git(&p.root, &["remote", "set-url", "origin", "https://github.com/acme/widgets.git"]);
    let sha = p.head();
    let (api, log) = github_mock(sha.clone());
    p.fx.db.set_setting(gitwork::API_BASE_KEY, &api).unwrap();
    gitwork::set_token(&p.fx.db, "github_pat_test123").unwrap();

    // Not enabled yet: no request is made and there are no links.
    let repo = p.repo();
    assert_eq!(repo.github_owner.as_deref(), Some("acme"));
    assert!(!repo.github_enabled);
    let r = p.report();
    assert!(r.github.is_none());
    assert!(log.lock().unwrap().is_empty());

    RepoStore::new(&p.fx.db).set_github(&repo.id, true).unwrap();
    let r = p.report();
    let gh = r.github.as_ref().expect("github info");
    assert_eq!(gh.commit_found, Some(true));
    assert_eq!(gh.commit_url, format!("https://github.com/acme/widgets/commit/{sha}"));
    assert_eq!(gh.pulls.len(), 1);
    assert_eq!(gh.pulls[0].url, "https://github.com/acme/widgets/pull/7");
    assert_eq!(gh.branches_containing, vec!["main".to_string()]);
    assert_eq!(r.facts.remote_update.status, "confirmed");
    assert!(r.facts.remote_update.label.contains("GitHub confirms"));
    assert_eq!(r.edits[0].outcome, "shipped_as_is");

    let reqs = log.lock().unwrap().clone();
    assert_eq!(reqs.len(), 3, "{reqs:?}");
    assert!(reqs.iter().all(|r| r.ends_with("Authorization: Bearer github_pat_test123")));
    assert!(reqs.iter().all(|r| !r.contains("SECRET") && !r.contains("speed")));
    let json = serde_json::to_string(&r).unwrap();
    assert!(!json.contains("SECRET-PROMPT-TEXT"));
    assert!(!json.contains("github_pat_test123"));
}

#[test]
fn github_unknown_commit_is_reported_not_hidden() {
    let p = Proj::new();
    p.agent_edit("tu-u", "main.rs", "let speed = 10;", "let speed = 31;");
    git(&p.root, &["commit", "-q", "-am", "local only"]);
    git(&p.root, &["remote", "set-url", "origin", "https://github.com/acme/widgets.git"]);
    let (api, _log) = github_mock("0000000000000000000000000000000000000000".into());
    p.fx.db.set_setting(gitwork::API_BASE_KEY, &api).unwrap();
    let repo = p.repo();
    RepoStore::new(&p.fx.db).set_github(&repo.id, true).unwrap();
    let r = p.report();
    assert_eq!(r.github.as_ref().unwrap().commit_found, Some(false));
    assert_eq!(r.facts.remote_update.status, "no");
}

#[test]
fn repo_store_rejects_non_repos_and_dedupes() {
    let p = Proj::new();
    let plain = p.fx.dir.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    assert!(RepoStore::new(&p.fx.db).add(plain.to_str().unwrap()).is_err());
    let a = RepoStore::new(&p.fx.db).add(&p.cwd()).unwrap();
    let b = RepoStore::new(&p.fx.db).add(p.root.join(".").to_str().unwrap()).unwrap();
    assert_eq!(a.id, b.id);
    assert_eq!(RepoStore::new(&p.fx.db).list().unwrap().len(), 1);
    assert!(RepoStore::new(&p.fx.db).set_github(&a.id, true).is_err(), "no GitHub remote yet");
    RepoStore::new(&p.fx.db).remove(&a.id).unwrap();
    assert!(RepoStore::new(&p.fx.db).list().unwrap().is_empty());
}
