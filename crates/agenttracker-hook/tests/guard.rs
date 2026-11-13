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
