//! Folder guard: stops AI helpers (Claude Code first) from changing folders the owner picked.
//!
//! The list lives in `<houndbit home>/guard.json` so the hook program can read it on its own,
//! even when the Houndbit window is closed. Each folder is either "block" (always stop changes)
//! or "ask" (wait for the owner's OK in Houndbit; no answer in time means stop).
//!
//! "Ask" requests are plain files: the hook writes `guard/asks/<id>.json` and waits for
//! `guard/asks/<id>.answer` ("allow" or "block"). Every decision is added to `guard/log.jsonl`.
//!
//! Limits (said plainly on the Protect page too): this checks the paths an AI helper names in its
//! request. A shell command can still reach a folder without naming it (a script, a variable, a
//! relative path from somewhere else). Shell commands that run *inside* a guarded folder are
//! treated as touching it.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GuardFolder {
    pub path: String,
    /// "block" or "ask".
    #[serde(default = "default_mode")]
    pub mode: String,
}

fn default_mode() -> String {
    "block".into()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GuardList {
    #[serde(default)]
    pub folders: Vec<GuardFolder>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Nothing guarded is involved.
    Allow,
    /// A guarded folder is involved. `mode` is that folder's setting.
    Guarded { folder: String, mode: String, target: String },
}

pub fn list_path(home: &Path) -> PathBuf {
    home.join("guard.json")
}
pub fn asks_dir(home: &Path) -> PathBuf {
    home.join("guard").join("asks")
}
pub fn log_path(home: &Path) -> PathBuf {
    home.join("guard").join("log.jsonl")
}

pub fn load(home: &Path) -> GuardList {
    std::fs::read_to_string(list_path(home)).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

/// Like `load`, but a file that exists and can't be read is an error (the hook then stops changes
/// rather than guessing the list is empty).
pub fn load_strict(home: &Path) -> Result<GuardList, String> {
    match std::fs::read_to_string(list_path(home)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(GuardList::default()),
        Err(e) => Err(e.to_string()),
        Ok(s) => serde_json::from_str(&s).map_err(|e| e.to_string()),
    }
}

/// True if the tool can change things (used when the list itself can't be read).
pub fn can_change(tool_name: &str) -> bool {
    !read_only_tool(tool_name)
}

pub fn save(home: &Path, list: &GuardList) -> std::io::Result<()> {
    std::fs::create_dir_all(home)?;
    let tmp = home.join("guard.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(list).unwrap_or_default())?;
    std::fs::rename(tmp, list_path(home))
}

/// Shows a folder the way people type it (drops the Windows `\\?\` prefix).
pub fn display_path(p: &str) -> String {
    if let Some(rest) = p.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    p.strip_prefix(r"\\?\").unwrap_or(p).to_string()
}

/// Lower-case, forward slashes, no repeated slashes, no trailing slash, no `\\?\` prefix.
pub fn norm(p: &str) -> String {
    let p = display_path(p.trim().trim_matches('"').trim_matches('\''));
    let mut out = String::with_capacity(p.len());
    let mut last_slash = false;
    for c in p.chars() {
        let c = if c == '\\' { '/' } else { c };
        if c == '/' {
            if last_slash {
                continue;
            }
            last_slash = true;
        } else {
            last_slash = false;
        }
        out.extend(c.to_lowercase());
    }
    while out.len() > 1 && out.ends_with('/') && !out.ends_with(":/") {
        out.pop();
    }
    out
}

/// Other spellings a shell might use for the same Windows folder (Git Bash `/f/test`, WSL `/mnt/f/test`).
fn spellings(folder_norm: &str) -> Vec<String> {
    let mut v = vec![folder_norm.to_string()];
    let b = folder_norm.as_bytes();
    if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        let drive = b[0] as char;
        let rest = &folder_norm[2..];
        v.push(format!("/{drive}{rest}"));
        v.push(format!("/mnt/{drive}{rest}"));
    }
    v
}

fn is_inside(path_norm: &str, folder_norm: &str) -> bool {
    if folder_norm.is_empty() {
        return false;
    }
    path_norm == folder_norm || path_norm.strip_prefix(folder_norm).is_some_and(|r| r.starts_with('/') || folder_norm.ends_with('/'))
}

/// True when `text` (already normalized) names the folder or something inside it.
fn mentions(text_norm: &str, folder_norm: &str) -> bool {
    for sp in spellings(folder_norm) {
        let mut from = 0;
        while let Some(i) = text_norm[from..].find(&sp) {
            let start = from + i;
            let end = start + sp.len();
            let before_ok = start == 0 || !text_norm[..start].ends_with(|c: char| c.is_alphanumeric() || c == '_' || c == '.' || c == '-');
            let after_ok = end == text_norm.len() || sp.ends_with('/') || text_norm[end..].starts_with(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.' || c == '-'));
            if before_ok && after_ok {
                return true;
            }
            from = end;
        }
    }
    false
}

/// Tools that only look at things. Everything else is treated as able to change files.
fn read_only_tool(name: &str) -> bool {
    const READ: &[&str] = &["read", "glob", "grep", "ls", "webfetch", "websearch", "todowrite", "todoread", "exitplanmode", "bashoutput", "notebookread", "listmcpresources", "readmcpresource"];
    let n = name.to_ascii_lowercase();
    if READ.contains(&n.as_str()) {
        return true;
    }
    // MCP tools are named mcp__<server>__<tool>; allow the ones that clearly only read.
    if let Some(tool) = n.strip_prefix("mcp__").and_then(|r| r.rsplit("__").next()) {
        return ["read", "list", "get", "search", "find", "view", "fetch"].iter().any(|p| tool.starts_with(p));
    }
    false
}

fn shell_tool(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "bash" || n.contains("powershell") || n.contains("shell") || n == "cmd" || n.contains("terminal") || n.contains("exec")
}

/// Every string in the request, with the name of the field it came from.
fn strings(key: &str, v: &Value, out: &mut Vec<(String, String)>) {
    match v {
        Value::String(s) => out.push((key.to_ascii_lowercase(), s.clone())),
        Value::Array(a) => a.iter().for_each(|x| strings(key, x, out)),
        Value::Object(o) => o.iter().for_each(|(k, x)| strings(k, x, out)),
        _ => {}
    }
}

/// Decides whether a Claude Code style tool request (`tool_name`, `tool_input`, `cwd`) touches a guarded folder.
pub fn check(list: &GuardList, tool_name: &str, tool_input: &Value, cwd: Option<&str>) -> Verdict {
    if list.folders.is_empty() || read_only_tool(tool_name) {
        return Verdict::Allow;
    }
    let mut texts = vec![];
    strings("", tool_input, &mut texts);
    let cwd_norm = cwd.map(norm);
    for f in &list.folders {
        let fnorm = norm(&f.path);
        if fnorm.is_empty() {
            continue;
        }
        let hit = |target: String| Verdict::Guarded { folder: display_path(&f.path), mode: f.mode.clone(), target };
        for (key, t) in &texts {
            let tn = norm(t);
            // A relative path in a path field is relative to cwd.
            let path_field = key.contains("path") || key.contains("file") || key.contains("dir");
            let joined = match &cwd_norm {
                Some(c) if path_field && !tn.is_empty() && !tn.starts_with('/') && !tn.contains(':') => format!("{c}/{tn}"),
                _ => tn.clone(),
            };
            if is_inside(&joined, &fnorm) || mentions(&tn, &fnorm) {
                return hit(short(t));
            }
        }
        if shell_tool(tool_name) {
            if let Some(c) = &cwd_norm {
                if is_inside(c, &fnorm) {
                    let cmd = texts.first().map(|(_, t)| t.clone()).unwrap_or_default();
                    return hit(format!("a command run inside {} ({})", display_path(&f.path), short(&cmd)));
                }
            }
        }
    }
    Verdict::Allow
}

fn short(s: &str) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() > 160 {
        format!("{}...", one_line.chars().take(160).collect::<String>())
    } else {
        one_line
    }
}

pub fn log(home: &Path, entry: Value) {
    use std::io::Write;
    let p = log_path(home);
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
        let _ = writeln!(f, "{entry}");
    }
}

/// Newest first, at most `n`.
pub fn recent_log(home: &Path, n: usize) -> Vec<Value> {
    let text = std::fs::read_to_string(log_path(home)).unwrap_or_default();
    text.lines().rev().filter_map(|l| serde_json::from_str(l).ok()).take(n).collect()
}

/// Ask requests still waiting (older than `max_age_ms` are ignored and tidied away).
pub fn pending_asks(home: &Path, max_age_ms: i64) -> Vec<Value> {
    let dir = asks_dir(home);
    let mut out = vec![];
    let now = crate::now_ms();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let Some(v) = std::fs::read_to_string(&p).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()) else { continue };
            if now - v.get("ts").and_then(Value::as_i64).unwrap_or(0) > max_age_ms {
                let _ = std::fs::remove_file(&p);
                continue;
            }
            if !p.with_extension("answer").exists() {
                out.push(v);
            }
        }
    }
    out.sort_by_key(|v| v.get("ts").and_then(Value::as_i64).unwrap_or(0));
    out
}

fn safe_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Owner's answer to an ask request. Returns false if the request is unknown or already answered.
pub fn answer(home: &Path, id: &str, allow: bool) -> bool {
    if !safe_id(id) {
        return false;
    }
    let dir = asks_dir(home);
    let ask = dir.join(format!("{id}.json"));
    let ans = dir.join(format!("{id}.answer"));
    if !ask.exists() || ans.exists() {
        return false;
    }
    std::fs::write(ans, if allow { "allow" } else { "block" }).is_ok()
}

/// Used by the hook: writes the request and waits for an answer. `None` means no answer in time.
pub fn ask_and_wait(home: &Path, id: &str, request: &Value, wait: std::time::Duration) -> Option<bool> {
    if !safe_id(id) {
        return None;
    }
    let dir = asks_dir(home);
    std::fs::create_dir_all(&dir).ok()?;
    let ask = dir.join(format!("{id}.json"));
    let ans = dir.join(format!("{id}.answer"));
    std::fs::write(&ask, request.to_string()).ok()?;
    let start = std::time::Instant::now();
    let result = loop {
        if let Ok(a) = std::fs::read_to_string(&ans) {
            break Some(a.trim() == "allow");
        }
        if start.elapsed() >= wait {
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    };
    let _ = std::fs::remove_file(&ask);
    let _ = std::fs::remove_file(&ans);
    result
}

/// What Houndbit shows for a decision.
pub fn log_entry(tool: &str, target: &str, folder: &str, decision: &str, why: &str) -> Value {
    json!({"ts": crate::now_ms(), "app": "Claude Code", "tool": tool, "target": target, "folder": folder, "decision": decision, "why": why})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(mode: &str) -> GuardList {
        GuardList { folders: vec![GuardFolder { path: r"\\?\F:\Test".into(), mode: mode.into() }] }
    }

    fn guarded(v: &Verdict) -> bool {
        matches!(v, Verdict::Guarded { .. })
    }

    #[test]
    fn file_tools_inside_folder_are_guarded() {
        let l = list("block");
        for tool in ["Write", "Edit", "MultiEdit", "NotebookEdit"] {
            assert!(guarded(&check(&l, tool, &json!({"file_path": r"f:\test\sigmadelta.txt", "content": "2 3 5"}), None)), "{tool}");
        }
        assert!(guarded(&check(&l, "Write", &json!({"file_path": "F:/Test/sub/a.txt"}), None)));
        assert!(guarded(&check(&l, "Write", &json!({"file_path": r"F:\Test"}), None)));
    }

    #[test]
    fn outside_and_read_only_are_allowed() {
        let l = list("block");
        assert_eq!(check(&l, "Write", &json!({"file_path": r"F:\Testing\a.txt"}), None), Verdict::Allow);
        assert_eq!(check(&l, "Write", &json!({"file_path": r"F:\Other\Test\a.txt"}), None), Verdict::Allow);
        assert_eq!(check(&l, "Read", &json!({"file_path": r"F:\Test\sigmadelta.txt"}), None), Verdict::Allow);
        assert_eq!(check(&l, "Grep", &json!({"path": r"F:\Test"}), None), Verdict::Allow);
        assert_eq!(check(&GuardList::default(), "Write", &json!({"file_path": r"F:\Test\a.txt"}), None), Verdict::Allow);
    }

    #[test]
    fn shell_commands_naming_the_folder_are_guarded() {
        let l = list("ask");
        // Roughly what Claude ran in the owner's test.
        let ps = r#"$primes = 2..10000 | Where-Object { ... }; Set-Content -Path 'f:\Test\sigmadelta.txt' -Value $primes"#;
        match check(&l, "PowerShell", &json!({"command": ps}), Some(r"C:\Users\Greg")) {
            Verdict::Guarded { mode, folder, .. } => {
                assert_eq!(mode, "ask");
                assert_eq!(folder, r"F:\Test");
            }
            v => panic!("{v:?}"),
        }
        assert!(guarded(&check(&l, "Bash", &json!({"command": "python gen.py > /f/Test/sigmadelta.txt"}), None)));
        assert!(guarded(&check(&l, "Bash", &json!({"command": "echo 1 >> /mnt/f/test/x.txt"}), None)));
        assert!(guarded(&check(&l, "Bash", &json!({"command": r#"echo 1 > "F:\\Test\\x.txt""#}), None)));
        assert_eq!(check(&l, "Bash", &json!({"command": r"dir F:\Testing"}), Some(r"C:\work")), Verdict::Allow);
    }

    #[test]
    fn shell_commands_run_inside_the_folder_are_guarded() {
        let l = list("block");
        assert!(guarded(&check(&l, "Bash", &json!({"command": "echo 7 > sigmadelta.txt"}), Some(r"F:\Test"))));
        assert!(guarded(&check(&l, "Write", &json!({"file_path": "sigmadelta.txt"}), Some(r"F:\Test"))));
        assert_eq!(check(&l, "Bash", &json!({"command": "echo hi"}), Some(r"F:\Work")), Verdict::Allow);
    }

    #[test]
    fn content_text_is_not_mistaken_for_a_path() {
        let l = list("block");
        assert_eq!(check(&l, "Write", &json!({"file_path": r"C:\Work\a.txt", "content": "hello"}), Some(r"F:\Test")), Verdict::Allow);
    }

    #[test]
    fn mcp_writes_guarded_reads_allowed() {
        let l = list("block");
        assert!(guarded(&check(&l, "mcp__filesystem__write_file", &json!({"path": r"F:\Test\a.txt"}), None)));
        assert_eq!(check(&l, "mcp__filesystem__read_file", &json!({"path": r"F:\Test\a.txt"}), None), Verdict::Allow);
    }

    #[test]
    fn display_and_norm() {
        assert_eq!(display_path(r"\\?\F:\Test"), r"F:\Test");
        assert_eq!(display_path(r"\\?\UNC\srv\share"), r"\\srv\share");
        assert_eq!(norm(r"F:\\Test\\"), "f:/test");
        assert_eq!(norm(r"F:\"), "f:/");
    }

    #[test]
    fn ask_answer_round_trip() {
        let t = std::env::temp_dir().join(format!("guard-test-{}", crate::now_ms()));
        let home = t.clone();
        let h2 = home.clone();
        let waiter = std::thread::spawn(move || ask_and_wait(&h2, "abc-1", &json!({"id": "abc-1", "ts": crate::now_ms()}), std::time::Duration::from_secs(10)));
        let mut answered = false;
        for _ in 0..80 {
            if pending_asks(&home, 60_000).iter().any(|v| v["id"] == "abc-1") {
                answered = answer(&home, "abc-1", false);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(answered);
        assert_eq!(waiter.join().unwrap(), Some(false));
        assert!(pending_asks(&home, 60_000).is_empty());
        assert!(!answer(&home, "../x", true));
        assert_eq!(ask_and_wait(&home, "abc-2", &json!({"ts": crate::now_ms()}), std::time::Duration::from_millis(300)), None);
        let _ = std::fs::remove_dir_all(t);
    }
}
