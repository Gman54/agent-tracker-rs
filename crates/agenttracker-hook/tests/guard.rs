//! Runs the real hook program the way Claude Code does (event JSON on stdin) and checks that it
//! stops changes to guarded folders and lets everything else through.
use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

fn run(home: &Path, payload: &Value) -> (i32, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_agenttracker-hook"))
        .args(["--connector", "claude_code", "--event", "PreToolUse"])
        .env("AGENTTRACKER_HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(payload.to_string().as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).into_owned())
}

fn pre(tool: &str, input: Value) -> Value {
    json!({"session_id": "s1", "cwd": "/home/greg", "hook_event_name": "PreToolUse", "tool_name": tool, "tool_input": input})
}

fn guard_file(home: &Path, folder: &str, mode: &str) {
    std::fs::create_dir_all(home).unwrap();
    std::fs::write(home.join("guard.json"), json!({"folders": [{"path": folder, "mode": mode}]}).to_string()).unwrap();
}

#[test]
fn blocks_writes_into_a_guarded_folder() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().join("home");
    guard_file(&home, r"F:\Test", "block");

    let (code, err) = run(&home, &pre("Write", json!({"file_path": r"f:\test\sigmadelta.txt", "content": "2\n3\n5"})));
    assert_eq!(code, 2, "{err}");
    assert!(err.contains(r"F:\Test") && err.contains("Houndbit Protect"), "{err}");

    let (code, _) = run(&home, &pre("PowerShell", json!({"command": r"Set-Content -Path 'F:\Test\sigmadelta.txt' -Value $p"})));
    assert_eq!(code, 2);

    // Reading is fine, and so is writing somewhere else.
    assert_eq!(run(&home, &pre("Read", json!({"file_path": r"F:\Test\sigmadelta.txt"}))).0, 0);
    assert_eq!(run(&home, &pre("Write", json!({"file_path": r"F:\Other\a.txt", "content": "x"}))).0, 0);

    let log = tracker_common::guard::recent_log(&home, 10);
    assert_eq!(log.len(), 2);
    assert_eq!(log[0]["decision"], "blocked");
}

#[test]
fn nothing_guarded_means_nothing_changes() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().join("home");
    assert_eq!(run(&home, &pre("Write", json!({"file_path": r"F:\Test\a.txt"}))).0, 0);
}

#[test]
fn unreadable_list_stops_changes_but_not_reads() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("guard.json"), "{ not json").unwrap();
    assert_eq!(run(&home, &pre("Write", json!({"file_path": "/tmp/a.txt"}))).0, 2);
    assert_eq!(run(&home, &pre("Read", json!({"file_path": "/tmp/a.txt"}))).0, 0);
}

#[test]
fn ask_folder_waits_for_the_owner() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().join("home");
    guard_file(&home, r"F:\Test", "ask");
    let h = home.clone();
    let hook = std::thread::spawn(move || run(&h, &pre("Edit", json!({"file_path": r"F:\Test\a.txt", "old_string": "a", "new_string": "b"}))));
    // Answer "yes" from the Houndbit side once the request shows up.
    let mut answered = false;
    for _ in 0..200 {
        if let Some(id) = tracker_common::guard::pending_asks(&home, 60_000).first().and_then(|v| v["id"].as_str().map(str::to_string)) {
            answered = tracker_common::guard::answer(&home, &id, true);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(answered);
    let (code, err) = hook.join().unwrap();
    assert_eq!(code, 0, "{err}");
    assert_eq!(tracker_common::guard::recent_log(&home, 1)[0]["decision"], "allowed");
}

fn run_cursor(home: &Path, event: &str, payload: &Value) -> (i32, Value) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_agenttracker-hook"))
        .args(["--connector", "cursor", "--event", event])
        .env("AGENTTRACKER_HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(payload.to_string().as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    (out.status.code().unwrap_or(-1), serde_json::from_str(text.trim()).unwrap_or(Value::Null))
}

#[test]
fn cursor_hooks_get_a_json_answer() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().join("home");
    guard_file(&home, r"F:\Test", "block");
    let common = |ev: &str| json!({"conversation_id": "c1", "generation_id": "g1", "hook_event_name": ev, "workspace_roots": ["/c:/work"]});

    let mut p = common("beforeShellExecution");
    p["command"] = json!(r"echo 1 > F:\Test\a.txt");
    p["cwd"] = json!("/c:/work");
    let (code, out) = run_cursor(&home, "beforeShellExecution", &p);
    assert_eq!(code, 0);
    assert_eq!(out["permission"], "deny", "{out}");
    assert!(out["agent_message"].as_str().unwrap().contains("Houndbit Protect"));

    // A command run inside the folder, Cursor-style path.
    let mut p = common("preToolUse");
    p["tool_name"] = json!("Shell");
    p["tool_input"] = json!({"command": "echo 1 > a.txt", "working_directory": "/f:/Test"});
    assert_eq!(run_cursor(&home, "preToolUse", &p).1["permission"], "deny");

    let mut p = common("preToolUse");
    p["tool_name"] = json!("Write");
    p["tool_input"] = json!({"file_path": "/f:/Test/a.txt", "contents": "x"});
    assert_eq!(run_cursor(&home, "preToolUse", &p).1["permission"], "deny");

    // MCP input may arrive as a JSON string.
    let mut p = common("beforeMCPExecution");
    p["tool_name"] = json!("write_file");
    p["tool_input"] = json!(json!({"path": r"F:\Test\a.txt"}).to_string());
    p["mcp_server_name"] = json!("filesystem");
    assert_eq!(run_cursor(&home, "beforeMCPExecution", &p).1["permission"], "deny");

    // Allowed things still get an explicit allow (no answer would block in Cursor).
    let mut p = common("preToolUse");
    p["tool_name"] = json!("Read");
    p["tool_input"] = json!({"file_path": "/f:/Test/a.txt"});
    assert_eq!(run_cursor(&home, "preToolUse", &p).1["permission"], "allow");
    let mut p = common("beforeShellExecution");
    p["command"] = json!("ls");
    p["cwd"] = json!("/c:/work");
    assert_eq!(run_cursor(&home, "beforeShellExecution", &p).1["permission"], "allow");

    // Report-only hooks print nothing.
    let mut p = common("afterFileEdit");
    p["file_path"] = json!("/f:/Test/a.txt");
    assert_eq!(run_cursor(&home, "afterFileEdit", &p).1, Value::Null);

    let log = tracker_common::guard::recent_log(&home, 10);
    assert_eq!(log.len(), 4);
    assert!(log.iter().all(|e| e["app"] == "Cursor"));
}

#[test]
fn switched_off_add_on_lets_everything_through() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("guard.json"), json!({"enabled": false, "folders": [{"path": r"F:\Test", "mode": "block"}]}).to_string()).unwrap();
    assert_eq!(run(&home, &pre("Write", json!({"file_path": r"F:\Test\a.txt"}))).0, 0);
    let p = json!({"hook_event_name": "beforeShellExecution", "command": r"del F:\Test\a.txt", "cwd": "/c:/work"});
    assert_eq!(run_cursor(&home, "beforeShellExecution", &p).1["permission"], "allow");
}

#[test]
fn cursor_ok_is_not_asked_twice() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().join("home");
    guard_file(&home, r"F:\Test", "ask");
    let pre_tool = json!({"hook_event_name": "preToolUse", "tool_name": "Shell", "tool_input": {"command": "echo 1 > a.txt", "working_directory": "/f:/Test"}, "cwd": "/f:/Test"});
    let h = home.clone();
    let p2 = pre_tool.clone();
    let hook = std::thread::spawn(move || run_cursor(&h, "preToolUse", &p2));
    let mut answered = false;
    for _ in 0..200 {
        if let Some(id) = tracker_common::guard::pending_asks(&home, 60_000).first().and_then(|v| v["id"].as_str().map(str::to_string)) {
            assert_eq!(tracker_common::guard::pending_asks(&home, 60_000)[0]["app"], "Cursor");
            answered = tracker_common::guard::answer(&home, &id, true);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(answered);
    assert_eq!(hook.join().unwrap().1["permission"], "allow");
    // The matching beforeShellExecution comes right after: no second question.
    let shell = json!({"hook_event_name": "beforeShellExecution", "command": "echo 1 > a.txt", "cwd": "/f:/Test"});
    let start = std::time::Instant::now();
    assert_eq!(run_cursor(&home, "beforeShellExecution", &shell).1["permission"], "allow");
    assert!(start.elapsed() < std::time::Duration::from_secs(5));
}
