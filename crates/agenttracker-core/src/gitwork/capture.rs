//! Records what agents do inside Git repositories, from the hook events AgentTracker already gets:
//! - file edits (with the agent's decision id, repo identity, path and before/after evidence)
//! - `git commit` / `git push` commands run through the agent's shell tool.
//! Read-only towards the repository: files are read and `git rev-parse` is run, nothing else.
use super::cli;
use crate::db::Db;
use crate::event::{AgentEvent, EventType};
use crate::Result;
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Tool names (lower-case) that edit one file.
const EDIT_TOOLS: &[&str] = &["edit", "multiedit", "write", "create", "create_file", "edit_file", "str_replace", "write_file", "replace_string_in_file"];
/// Tool names (lower-case) that run shell commands.
const SHELL_TOOLS: &[&str] = &["bash", "shell", "run_in_terminal", "powershell", "run_terminal_cmd", "terminal"];

pub fn tracking_on(db: &Db) -> bool {
    db.get_setting("git_tracking").ok().flatten().map(|v| v != "0" && v != "false").unwrap_or(true)
}

fn input_of(p: &Value) -> Value {
    match p.get("input") {
        Some(Value::String(s)) => serde_json::from_str(s).unwrap_or(Value::Null),
        Some(v) => v.clone(),
        None => Value::Null,
    }
}

fn file_from_input(input: &Value) -> Option<String> {
    ["file_path", "path", "filePath", "target_file"].iter().find_map(|k| input.get(*k).and_then(Value::as_str)).map(str::to_string).filter(|s| !s.trim().is_empty() && !s.contains("[REDACTED"))
}

fn absolute(file: &str, cwd: Option<&str>) -> Option<PathBuf> {
    let p = Path::new(file);
    if p.is_absolute() || file.starts_with('/') || file.starts_with('\\') {
        Some(p.to_path_buf())
    } else {
        cwd.map(|c| Path::new(c).join(p))
    }
}

/// Called synchronously when a hook delivery arrives (before Claude Code runs the tool), so the
/// file can be read in its "before" state. Only Claude Code PreToolUse for edit tools.
pub fn pre_snapshot(db: &Db, envelope: &Value) {
    if envelope.get("connector").and_then(Value::as_str) != Some("claude_code") || !tracking_on(db) {
        return;
    }
    let p = match envelope.get("payload") {
        Some(p) => p,
        None => return,
    };
    let name = p.get("hook_event_name").and_then(Value::as_str).or_else(|| envelope.get("hook_event").and_then(Value::as_str));
    if name != Some("PreToolUse") {
        return;
    }
    let tool = p.get("tool_name").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
    if !EDIT_TOOLS.contains(&tool.as_str()) {
        return;
    }
    let (Some(tcid), Some(file)) = (p.get("tool_use_id").and_then(Value::as_str), p.get("tool_input").and_then(file_from_input)) else { return };
    let Some(abs) = absolute(&file, p.get("cwd").and_then(Value::as_str)) else { return };
    if cli::find_repo_root(&abs).is_none() {
        return;
    }
    let bytes = std::fs::read(&abs).ok();
    let _ = db.conn().execute(
        "INSERT OR IGNORE INTO git_pre_snapshots(tool_call_id, abs_path, existed, content, blob, captured_at) VALUES (?1,?2,?3,?4,?5,?6)",
        params![
            tcid,
            abs.to_string_lossy(),
            bytes.is_some(),
            bytes.as_deref().and_then(cli::as_text).map(|t| cli::scrub(&t)),
            bytes.as_deref().map(cli::blob_id),
            crate::now_ms()
        ],
    );
}

/// One requested change: replace `old` with `new` (old empty for whole-file writes).
fn proposals(tool: &str, input: &Value, cursor_data: Option<&Value>) -> Vec<Value> {
    let pair = |o: &Value, n: &Value| json!({"old": o.as_str().unwrap_or(""), "new": n.as_str().unwrap_or("")});
    if let Some(d) = cursor_data {
        return d.get("edits").and_then(Value::as_array).map(|a| a.iter().map(|e| pair(&e["old_string"], &e["new_string"])).collect()).unwrap_or_default();
    }
    match tool {
        "multiedit" => input.get("edits").and_then(Value::as_array).map(|a| a.iter().map(|e| pair(&e["old_string"], &e["new_string"])).collect()).unwrap_or_default(),
        "write" | "create" | "create_file" | "write_file" => vec![json!({"old": "", "new": input.get("content").and_then(Value::as_str).unwrap_or(""), "whole_file": true})],
        _ => {
            let o = input.get("old_string").or_else(|| input.get("oldString")).or_else(|| input.get("old_str")).cloned().unwrap_or(Value::Null);
            let n = input.get("new_string").or_else(|| input.get("newString")).or_else(|| input.get("new_str")).cloned().unwrap_or(Value::Null);
            if o.is_null() && n.is_null() {
                vec![]
            } else {
                vec![pair(&o, &n)]
            }
        }
    }
}

/// Called after an event is stored. Never fails the caller; problems are logged.
pub fn observe(db: &Db, ev: &AgentEvent) {
    if ev.event_type != EventType::ToolFinished || !tracking_on(db) {
        return;
    }
    if let Err(e) = observe_inner(db, ev) {
        eprintln!("git tracking: {e}");
    }
}

fn observe_inner(db: &Db, ev: &AgentEvent) -> Result<()> {
    let p = &ev.payload_redacted;
    let failed = p.get("failed").and_then(Value::as_bool).unwrap_or(false);
    let cursor_hook = p.get("hook").and_then(Value::as_str);
    let input = input_of(p);
    let tool = p.get("tool").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
    let cwd = ev.working_dir.as_deref();

    // ---- file edits
    let (edit_file, cursor_data, tool_label) = if cursor_hook == Some("afterFileEdit") {
        (p.pointer("/data/file_path").and_then(Value::as_str).map(str::to_string), p.get("data"), "Cursor file edit".to_string())
    } else if EDIT_TOOLS.contains(&tool.as_str()) {
        (file_from_input(&input), None, p.get("tool").and_then(Value::as_str).unwrap_or("").to_string())
    } else {
        (None, None, String::new())
    };
    if let (Some(file), false) = (edit_file, failed) {
        if let Some(abs) = absolute(&file, cwd) {
            record_edit(db, ev, &abs, &tool, &tool_label, &input, cursor_data)?;
        }
        return Ok(());
    }

    // ---- shell commands
    let command = if SHELL_TOOLS.contains(&tool.as_str()) {
        input.get("command").and_then(Value::as_str).map(str::to_string)
    } else if cursor_hook == Some("afterShellExecution") {
        p.pointer("/data/command").and_then(Value::as_str).map(str::to_string)
    } else {
        None
    };
    if let Some(cmd) = command {
        record_git_commands(db, ev, &cmd, failed)?;
    }
    Ok(())
}

fn record_edit(db: &Db, ev: &AgentEvent, abs: &Path, tool: &str, tool_label: &str, input: &Value, cursor_data: Option<&Value>) -> Result<()> {
    let Some(root) = cli::find_repo_root(abs) else { return Ok(()) };
    let Some(rel) = cli::rel_in_repo(&root, abs) else { return Ok(()) };
    if rel.starts_with(".git/") {
        return Ok(());
    }
    let root_s = cli::try_line(&root, &["rev-parse", "--show-toplevel"]).unwrap_or_else(|| root.to_string_lossy().into_owned());
    let (identity, _) = cli::repo_identity(&root);
    let decision_id = ev.tool_call_id.clone().unwrap_or_else(|| ev.event_id.clone());
    let props = proposals(tool, input, cursor_data);

    // Before: our own snapshot taken before the tool ran is best; else what the agent reported.
    let pre: Option<(Option<String>, Option<String>)> = ev.tool_call_id.as_ref().and_then(|t| {
        db.conn().query_row("SELECT content, blob FROM git_pre_snapshots WHERE tool_call_id=?1", [t], |r| Ok((r.get(0)?, r.get(1)?))).optional().ok().flatten()
    });
    let reported = ev.payload_redacted.pointer("/response/originalFile").and_then(Value::as_str).map(str::to_string);
    let (before_source, before_file, before_blob) = match (pre, reported) {
        (Some((c, b)), _) => ("pre_hook_snapshot", c, b),
        (None, Some(r)) => ("agent_report", Some(r), None),
        _ => ("none", None, None),
    };
    let bytes = std::fs::read(abs).ok();
    let after_file = bytes.as_deref().and_then(cli::as_text).map(|t| cli::scrub(&t));
    let after_blob = bytes.as_deref().map(cli::blob_id);
    let user_modified = ev.payload_redacted.pointer("/response/userModified").and_then(Value::as_bool);
    db.conn().execute(
        "INSERT OR IGNORE INTO git_agent_edits(id, decision_id, decision_source, event_id, session_id, task_id, connector, tool, repo_root, repo_identity, rel_path, proposal,
            before_source, before_file, before_blob, after_file, after_blob, head_at_edit, branch_at_edit, user_modified, observed_at)
         VALUES (?1,?2,'hook_event',?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)",
        params![
            format!("ged-{}", uuid::Uuid::new_v4()),
            decision_id,
            ev.event_id,
            ev.session_id,
            ev.task_id,
            ev.connector_name,
            tool_label,
            root_s,
            identity,
            rel,
            Value::Array(props).to_string(),
            before_source,
            before_file,
            before_blob,
            after_file,
            after_blob,
            cli::try_line(&root, &["rev-parse", "HEAD"]),
            cli::try_line(&root, &["rev-parse", "--abbrev-ref", "HEAD"]),
            user_modified,
            ev.collector_ts
        ],
    )?;
    Ok(())
}

/// Finds `git commit` / `git push` in a shell command line (handles `&&`, `;`, `|`, `git -C dir`).
/// Returns (kind, the -C folder if any).
pub fn git_subcommands(cmd: &str) -> Vec<(&'static str, Option<String>)> {
    let mut out = vec![];
    let mut cd_dir: Option<String> = None;
    for part in cmd.split(['&', ';', '|', '\n']) {
        let toks: Vec<&str> = part.split_whitespace().collect();
        // The command word must come first (after any VAR=value prefixes), so "echo git push" doesn't count.
        let Some(gi) = toks.iter().position(|t| !t.contains('=')) else { continue };
        let first = toks[gi].trim_matches(['"', '\'', '(']);
        if (first == "cd" || first == "Set-Location" || first == "pushd") && gi + 1 < toks.len() {
            cd_dir = Some(toks[gi + 1].trim_matches(['"', '\'']).to_string());
            continue;
        }
        if !(first == "git" || first == "git.exe" || first.ends_with("/git") || first.ends_with("\\git.exe") || first.ends_with("\\git")) {
            continue;
        }
        let mut i = gi + 1;
        let mut dir = None;
        while i < toks.len() && toks[i].starts_with('-') {
            if toks[i] == "-C" && i + 1 < toks.len() {
                dir = Some(toks[i + 1].trim_matches(['"', '\'']).to_string());
                i += 2;
            } else if toks[i] == "-c" {
                i += 2;
            } else {
                i += 1;
            }
        }
        let dir = dir.or_else(|| cd_dir.clone());
        match toks.get(i).copied() {
            Some("commit") => out.push(("commit", dir)),
            Some("push") => out.push(("push", dir)),
            _ => {}
        }
    }
    out
}

fn record_git_commands(db: &Db, ev: &AgentEvent, cmd: &str, failed: bool) -> Result<()> {
    let found = git_subcommands(cmd);
    if found.is_empty() {
        return Ok(());
    }
    let resp = ev.payload_redacted.get("response").cloned().unwrap_or(Value::Null);
    let resp_text = match &resp {
        Value::String(s) => s.clone(),
        v => format!("{} {}", v.get("stderr").and_then(Value::as_str).unwrap_or(""), v.get("stdout").and_then(Value::as_str).unwrap_or("")),
    };
    let exit = resp.get("exit_code").or_else(|| resp.get("exitCode")).and_then(Value::as_i64).or_else(|| ev.payload_redacted.pointer("/data/exit_code").and_then(Value::as_i64));
    let looks_failed = ["! [rejected]", "fatal:", "error: failed to push", "nothing to commit"].iter().any(|m| resp_text.contains(m));
    let interrupted = resp.get("interrupted").and_then(Value::as_bool).unwrap_or(false);
    // Claude Code reports a failing command as PostToolUseFailure ("failed"); other agents may give an exit code.
    let succeeded: Option<bool> = if failed || interrupted || looks_failed || exit.map(|c| c != 0).unwrap_or(false) {
        Some(false)
    } else if ev.connector_name == "claude_code" || exit == Some(0) {
        Some(true)
    } else {
        None
    };
    let mut short: String = cli::scrub(cmd).chars().take(400).collect();
    if cmd.chars().count() > 400 {
        short.push_str("...");
    }
    for (kind, dir) in found {
        let base = match (&dir, ev.working_dir.as_deref()) {
            (Some(d), cwd) => absolute(d, cwd),
            (None, Some(cwd)) => Some(PathBuf::from(cwd)),
            _ => None,
        };
        let Some(root) = base.as_deref().and_then(cli::find_repo_root) else { continue };
        let root_s = cli::try_line(&root, &["rev-parse", "--show-toplevel"]).unwrap_or_else(|| root.to_string_lossy().into_owned());
        let (identity, _) = cli::repo_identity(&root);
        db.conn().execute(
            "INSERT OR IGNORE INTO git_command_facts(id, event_id, session_id, connector, kind, repo_root, repo_identity, command, succeeded, head_after, branch_after, observed_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![
                format!("gcf-{}", uuid::Uuid::new_v4()),
                ev.event_id,
                ev.session_id,
                ev.connector_name,
                kind,
                root_s,
                identity,
                short,
                succeeded,
                cli::try_line(&root, &["rev-parse", "HEAD"]),
                cli::try_line(&root, &["rev-parse", "--abbrev-ref", "HEAD"]),
                ev.collector_ts
            ],
        )?;
    }
    Ok(())
}

/// Copies successful `write_text_file` actions from the decision runtime into the edit table,
/// keyed by the runtime's own decision id. Safe to call repeatedly.
pub fn import_runtime_edits(db: &Db) -> Result<usize> {
    let rows: Vec<(String, String, String, String, i64, Option<String>)> = {
        let conn = db.conn();
        let mut st = conn.prepare(
            "SELECT d.id, a.session_id, a.args, s.working_dir, a.created_at, r.status FROM rt_actions a
               JOIN rt_decisions d ON d.id = a.decision_id JOIN rt_sessions s ON s.id = a.session_id
               LEFT JOIN rt_results r ON r.action_id = a.id
              WHERE a.tool = 'write_text_file'
                AND NOT EXISTS (SELECT 1 FROM git_agent_edits g WHERE g.decision_id = d.id)",
        )?;
        let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?.collect::<std::result::Result<Vec<_>, _>>()?;
        v
    };
    let mut n = 0;
    for (decision_id, session_id, args, wd, at, status) in rows {
        if status.as_deref() != Some("succeeded") {
            continue;
        }
        let a: Value = serde_json::from_str(&args).unwrap_or(Value::Null);
        let (Some(path), content) = (a.get("path").and_then(Value::as_str), a.get("content").and_then(Value::as_str).unwrap_or("")) else { continue };
        let abs = Path::new(&wd).join(path);
        let Some(root) = cli::find_repo_root(&abs) else { continue };
        let Some(rel) = cli::rel_in_repo(&root, &abs) else { continue };
        let root_s = cli::try_line(&root, &["rev-parse", "--show-toplevel"]).unwrap_or_else(|| root.to_string_lossy().into_owned());
        let (identity, _) = cli::repo_identity(&root);
        let before_sha: Option<String> = db.conn().query_row(
            "SELECT c.before_sha256 FROM rt_changes c JOIN rt_actions a ON a.id = c.action_id WHERE a.decision_id = ?1 LIMIT 1", [&decision_id], |r| r.get(0)).optional()?.flatten();
        db.conn().execute(
            "INSERT OR IGNORE INTO git_agent_edits(id, decision_id, decision_source, event_id, session_id, task_id, connector, tool, repo_root, repo_identity, rel_path, proposal,
                before_source, before_file, before_blob, after_file, after_blob, head_at_edit, branch_at_edit, user_modified, observed_at)
             VALUES (?1,?2,'runtime_decision',NULL,?3,NULL,'agent_runtime','write_text_file',?4,?5,?6,?7,'none',NULL,?8,?9,?10,NULL,NULL,NULL,?11)",
            params![
                format!("ged-{}", uuid::Uuid::new_v4()),
                decision_id,
                session_id,
                root_s,
                identity,
                rel,
                json!([{"old": "", "new": content, "whole_file": true}]).to_string(),
                before_sha.map(|s| format!("sha256:{s}")),
                content,
                cli::blob_id(content.as_bytes()),
                at
            ],
        )?;
        n += 1;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_git_commands_in_shell_lines() {
        assert_eq!(git_subcommands("git add -A && git commit -m 'x' && git push origin main"), vec![("commit", None), ("push", None)]);
        assert_eq!(git_subcommands("git -C ../repo push"), vec![("push", Some("../repo".into()))]);
        assert_eq!(git_subcommands("git -c user.name=x commit -m y"), vec![("commit", None)]);
        assert!(git_subcommands("echo git push is fun").is_empty());
        assert_eq!(git_subcommands("cd repo && GIT_TRACE=0 git push"), vec![("push", Some("repo".into()))]);
        assert!(git_subcommands("git status; git log").is_empty());
        assert!(git_subcommands("cargo build").is_empty());
    }
}
