//! AgentTracker acceptance tests (spec §15, automatable items).
//!
//! Every test here is a **fake-agent contract test**: agent activity is simulated with
//! hand-built hook envelopes (documented Claude Code hook JSON) and ordinary file operations.
//! They prove the collector/verifier contract, NOT that a live agent integration works.
use agenttracker_acceptance::{check, claude_envelope, Fixture};
use agenttracker_core::connectors::claude_code::ClaudeConnector;
use agenttracker_core::db::{Db, Migrator};
use agenttracker_core::event::{AgentEvent, CaptureSource, EventType};
use agenttracker_core::fs_observer::{EntryType, ManifestType, Snapshotter, Stability};
use agenttracker_core::queue::Queue;
use agenttracker_core::verifiers::{CheckStatus, Evidence};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tracker_common::hash_chain::ChainStore;

fn status_of(run: &agenttracker_core::store::VerificationRun, check_type: &str) -> CheckStatus {
    run.results.iter().find(|r| r.check_type == check_type).unwrap_or_else(|| panic!("no result for {check_type}")).status
}

fn copy_all(work: &Path) {
    for e in std::fs::read_dir(work.join("input")).unwrap() {
        let e = e.unwrap();
        std::fs::copy(e.path(), work.join("output").join(e.file_name())).unwrap();
    }
}

// fake-agent contract test
#[test]
fn test_successful_file_copy_task() {
    let fx = Fixture::new();
    let work = fx.workspace(&[("a.txt", "alpha"), ("b.txt", "beta"), ("c.txt", "gamma")]);
    let task = fx.copy_task(&work, &["a.txt", "b.txt", "c.txt"], None);
    fx.engine.capture_baseline(&task.id).unwrap();
    fx.engine.start(&task.id).unwrap();

    let cwd = work.to_string_lossy().to_string();
    fx.deliver(claude_envelope("SessionStart", 1_000, json!({"session_id": "s-ok", "cwd": cwd, "source": "startup"})));
    fx.deliver(claude_envelope("PreToolUse", 2_000, json!({"session_id": "s-ok", "cwd": cwd, "tool_name": "Bash", "tool_input": {"command": "cp input/* output/"}})));
    copy_all(&work);
    fx.deliver(claude_envelope("PostToolUse", 3_000, json!({"session_id": "s-ok", "cwd": cwd, "tool_name": "Bash", "tool_response": {"ok": true}})));
    fx.deliver(claude_envelope("Stop", 4_000, json!({"session_id": "s-ok", "cwd": cwd})));

    let run = fx.engine.verify(&task.id).unwrap();
    assert_eq!(run.overall_verdict, Some(CheckStatus::Passed), "{:#?}", run.results);
    let t = fx.engine.tasks.get(&task.id).unwrap();
    assert_eq!(t.session_id.as_deref(), Some("s-ok"), "events attributed to the running task by cwd");
    let evs = fx.engine.events.by_task(&task.id).unwrap();
    assert_eq!(evs.len(), 4);
    assert_eq!(evs.last().unwrap().event_type, EventType::AgentReportedCompletion);
}

// fake-agent contract test
#[test]
fn test_missing_output() {
    let fx = Fixture::new();
    let work = fx.workspace(&[("a.txt", "alpha")]);
    let task = fx.copy_task(&work, &["a.txt"], None);
    fx.engine.capture_baseline(&task.id).unwrap();
    fx.engine.start(&task.id).unwrap();
    // The agent claims success but never writes the file.
    fx.deliver(claude_envelope("Stop", 1, json!({"session_id": "s-miss", "cwd": work.to_string_lossy()})));
    let run = fx.engine.verify(&task.id).unwrap();
    assert_eq!(status_of(&run, "required_outputs_exist"), CheckStatus::Failed);
    assert_eq!(run.overall_verdict, Some(CheckStatus::Failed));
}

// fake-agent contract test
#[test]
fn test_protected_original_modified() {
    let fx = Fixture::new();
    let work = fx.workspace(&[("a.txt", "alpha"), ("b.txt", "beta")]);
    let task = fx.copy_task(&work, &["a.txt", "b.txt"], None);
    fx.engine.capture_baseline(&task.id).unwrap();
    fx.engine.start(&task.id).unwrap();
    copy_all(&work);
    std::fs::write(work.join("input/a.txt"), "alpha EDITED").unwrap();
    let run = fx.engine.verify(&task.id).unwrap();
    assert_eq!(status_of(&run, "originals_unchanged"), CheckStatus::Failed);
    assert_eq!(status_of(&run, "changes_within_scope"), CheckStatus::Failed);
    assert_eq!(run.overall_verdict, Some(CheckStatus::Failed));
}

// fake-agent contract test
#[test]
fn test_duplicate_inputs_too_few_outputs() {
    let fx = Fixture::new();
    // Two inputs with identical content need two matching outputs (hash multiplicity).
    let work = fx.workspace(&[("one.txt", "same content"), ("two.txt", "same content")]);
    let task = fx.copy_task(&work, &[], Some(vec![check("inputs_preserved_in_output", json!({"input": "input", "output": "output"}), true)]));
    fx.engine.capture_baseline(&task.id).unwrap();
    fx.engine.start(&task.id).unwrap();
    std::fs::copy(work.join("input/one.txt"), work.join("output/one.txt")).unwrap();
    let run = fx.engine.verify(&task.id).unwrap();
    assert_eq!(status_of(&run, "inputs_preserved_in_output"), CheckStatus::Failed, "{:#?}", run.results);

    // Adding the second copy (any name) satisfies it.
    let task2 = fx.copy_task(&work, &[], Some(vec![check("inputs_preserved_in_output", json!({"input": "input", "output": "output"}), true)]));
    fx.engine.capture_baseline(&task2.id).unwrap();
    std::fs::copy(work.join("input/two.txt"), work.join("output/renamed.txt")).unwrap();
    let run2 = fx.engine.verify(&task2.id).unwrap();
    assert_eq!(status_of(&run2, "inputs_preserved_in_output"), CheckStatus::Passed, "{:#?}", run2.results);
}

// fake-agent contract test
#[test]
fn test_unreadable_locked_files() {
    let fx = Fixture::new();
    let work = fx.workspace(&[("a.txt", "alpha"), ("locked.txt", "secret")]);
    let locked = work.join("input/locked.txt");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&locked).is_ok() {
            eprintln!("SKIPPED: running with privileges that ignore file permissions (root)");
            return;
        }
    }
    #[cfg(windows)]
    let _lock = {
        // Exclusive share mode = the file is locked for other readers.
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new().read(true).share_mode(0).open(&locked).unwrap()
    };
    let task = fx.copy_task(&work, &[], Some(vec![check("originals_unchanged", json!({}), true)]));
    fx.engine.capture_baseline(&task.id).unwrap();
    let base = fx.engine.manifests.latest_for_task(&task.id, ManifestType::Baseline).unwrap();
    let entry = base.iter().find_map(|m| m.entries.iter().find(|e| e.rel_path.ends_with("locked.txt"))).expect("locked file listed");
    assert!(!entry.read_ok, "unreadable file must be recorded as read_ok=false");
    let run = fx.engine.verify(&task.id).unwrap();
    assert_eq!(status_of(&run, "originals_unchanged"), CheckStatus::UnableToVerify, "{:#?}", run.results);
    assert_eq!(run.overall_verdict, Some(CheckStatus::UnableToVerify));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644));
    }
}

// fake-agent contract test
#[test]
fn test_files_changing_during_capture() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("growing.log"), "start\n").unwrap();
    std::fs::write(dir.path().join("calm.txt"), "still").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let snap = Snapshotter {
        // Simulates another process appending every time the snapshotter finishes hashing.
        after_hash_hook: Some(Arc::new(move |p: &Path| {
            if p.file_name().map(|n| n == "growing.log").unwrap_or(false) {
                c.fetch_add(1, Ordering::SeqCst);
                use std::io::Write;
                let mut f = std::fs::OpenOptions::new().append(true).open(p).unwrap();
                writeln!(f, "more data {}", c.load(Ordering::SeqCst)).unwrap();
            }
        })),
    };
    let m = snap.capture_with(dir.path(), &[], ManifestType::Baseline, None);
    let growing = m.get("growing.log").unwrap();
    assert_eq!(growing.stability, Stability::Unstable);
    assert!(!growing.trustworthy());
    assert!(calls.load(Ordering::SeqCst) >= 2, "retried before giving up");
    assert_eq!(m.get("calm.txt").unwrap().stability, Stability::Stable);
}

// fake-agent contract test
#[cfg(unix)]
#[test]
fn test_symlink_escaping_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("private.txt"), "do not read").unwrap();
    std::fs::write(root.join("normal.txt"), "ok").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
    std::os::unix::fs::symlink(outside.join("private.txt"), root.join("file_link")).unwrap();
    let m = Snapshotter::capture(&root, &[]);
    assert!(m.entries.iter().all(|e| !e.rel_path.contains("private.txt")), "must not traverse outside root: {:#?}", m.entries);
    let esc = m.get("escape").expect("symlink recorded");
    assert_eq!(esc.entry_type, EntryType::Symlink);
    assert!(esc.sha256.is_none());
    assert_eq!(m.get("file_link").unwrap().entry_type, EntryType::Symlink);
    assert!(m.get("normal.txt").unwrap().read_ok);
}

// Windows variant: directory symlinks need privileges, so use a junction-free check that the
// observer records reparse points without following them when creation succeeds.
#[cfg(windows)]
#[test]
fn test_symlink_escaping_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("private.txt"), "do not read").unwrap();
    if std::os::windows::fs::symlink_dir(&outside, root.join("escape")).is_err() {
        eprintln!("SKIPPED: creating symlinks needs Developer Mode or admin rights");
        return;
    }
    let m = Snapshotter::capture(&root, &[]);
    assert!(m.entries.iter().all(|e| !e.rel_path.contains("private.txt")));
    assert_eq!(m.get("escape").unwrap().entry_type, EntryType::Symlink);
}

// fake-agent contract test
#[test]
fn test_duplicate_events_deduplicated() {
    let fx = Fixture::new();
    let env = claude_envelope("PreToolUse", 42, json!({"session_id": "s-dup", "tool_name": "Read", "tool_input": {"file_path": "x"}}));
    // Same delivery arrives twice (e.g. HTTP retry + fallback-file replay).
    fx.deliver(env.clone());
    fx.deliver(env);
    assert_eq!(fx.engine.events.by_session("s-dup").unwrap().len(), 1);
    assert_eq!(fx.engine.events.count().unwrap(), 1);
    assert_eq!(fx.collector.queue.pending_count().unwrap(), 0, "both deliveries were consumed");
    // Also at the store level.
    let ev = AgentEvent::new("manual", "t", EventType::ToolStarted, CaptureSource::Manual, Some("fixed-id".into()));
    assert!(fx.engine.events.insert(&ev).unwrap());
    assert!(!fx.engine.events.insert(&ev).unwrap());
}

// fake-agent contract test
#[test]
fn test_out_of_order_events_ordered_by_source_seq() {
    let fx = Fixture::new();
    let p = |n: &str| json!({"session_id": "s-ord", "tool_name": n, "tool_input": {}});
    // Delivered 3, 1, 2 — source order is 1, 2, 3.
    fx.deliver(claude_envelope("PreToolUse", 3_000_000, p("third")));
    fx.deliver(claude_envelope("PreToolUse", 1_000_000, p("first")));
    fx.deliver(claude_envelope("PreToolUse", 2_000_000, p("second")));
    let names: Vec<String> = fx
        .engine
        .events
        .by_session("s-ord")
        .unwrap()
        .iter()
        .map(|e| e.payload_redacted["tool"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, ["first", "second", "third"]);

    // The pure sort uses source_seq before collector_ts.
    let mut evs: Vec<AgentEvent> = [(2, 10), (1, 20)]
        .iter()
        .map(|(seq, cts)| {
            let mut e = AgentEvent::new("x", "t", EventType::ToolStarted, CaptureSource::Hook, Some(format!("e{seq}")));
            e.source_seq = Some(*seq);
            e.collector_ts = *cts;
            e
        })
        .collect();
    agenttracker_core::event::sort_source_order(&mut evs);
    assert_eq!(evs[0].event_id, "e1");
}

// fake-agent contract test
#[test]
fn test_missing_baseline() {
    let fx = Fixture::new();
    let work = fx.workspace(&[("a.txt", "alpha")]);
    let task = fx.copy_task(&work, &["a.txt"], Some(vec![check("originals_unchanged", json!({}), true), check("changes_within_scope", json!({}), true)]));
    copy_all(&work);
    // Evidence with an "after" snapshot but no "before" snapshot.
    let after = Snapshotter::new().capture_with(&work, &[], ManifestType::Verification, Some(&task.id));
    let ev = Evidence { task: Some(task.clone()), verifications: vec![after], ..Default::default() };
    let run = fx.engine.run_checks(&task, &ev, 0);
    assert_eq!(status_of(&run, "originals_unchanged"), CheckStatus::UnableToVerify);
    assert_eq!(status_of(&run, "changes_within_scope"), CheckStatus::UnableToVerify);
    assert_eq!(run.overall_verdict, Some(CheckStatus::UnableToVerify));
    // And the lifecycle refuses to verify a task that was never baselined.
    assert!(fx.engine.verify(&task.id).is_err());
}

// fake-agent contract test
#[test]
fn test_no_required_checks() {
    let fx = Fixture::new();
    let work = fx.workspace(&[("a.txt", "alpha")]);
    // Only an optional check — even if it passes, the verdict can't be "passed".
    let task = fx.copy_task(&work, &[], Some(vec![check("originals_unchanged", json!({}), false)]));
    fx.engine.capture_baseline(&task.id).unwrap();
    let run = fx.engine.verify(&task.id).unwrap();
    assert_eq!(status_of(&run, "originals_unchanged"), CheckStatus::Passed);
    assert_eq!(run.overall_verdict, Some(CheckStatus::UnableToVerify));

    let empty = fx.copy_task(&work, &[], Some(vec![]));
    fx.engine.capture_baseline(&empty.id).unwrap();
    assert_eq!(fx.engine.verify(&empty.id).unwrap().overall_verdict, Some(CheckStatus::UnableToVerify));
}

// fake-agent contract test
#[test]
fn test_secret_redaction_in_stored_events() {
    let fx = Fixture::new();
    let secret_key = format!("sk-ant-{}", "a1b2c3d4e5f6g7h8i9j0k1l2m3n4o5p6");
    let gh = format!("ghp_{}", "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789");
    fx.deliver(claude_envelope(
        "PreToolUse",
        7,
        json!({"session_id": "s-sec", "tool_name": "Bash", "tool_input": {"command": format!("curl -H 'Authorization: Bearer {secret_key}' x"), "password": "hunter2-very-secret", "note": gh}}),
    ));
    // Inspect the raw stored row, not just the API.
    let conn = fx.db.conn();
    let stored: String = conn.query_row("SELECT payload_redacted FROM events WHERE session_id = 's-sec'", [], |r| r.get(0)).unwrap();
    assert!(!stored.contains(&secret_key), "API key leaked: {stored}");
    assert!(!stored.contains(&gh), "GitHub token leaked: {stored}");
    assert!(!stored.contains("hunter2-very-secret"), "password leaked: {stored}");
    assert!(stored.contains(tracker_common::redaction::REDACTED));
    // The durable queue row and the hash-chain canonical copy must be redacted too.
    let queued: String = conn.query_row("SELECT group_concat(payload) FROM queue", [], |r| r.get(0)).unwrap();
    let chained: String = conn.query_row("SELECT group_concat(event_canonical) FROM chain_entries", [], |r| r.get(0)).unwrap();
    for (what, text) in [("queue", &queued), ("chain", &chained)] {
        assert!(!text.contains(&secret_key) && !text.contains(&gh) && !text.contains("hunter2-very-secret"), "{what} leaked: {text}");
    }
}

fn settings_with_user_config(dir: &Path) -> std::path::PathBuf {
    let p = dir.join(".claude/settings.json");
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    let user = json!({
        "model": "opus",
        "permissions": {"allow": ["Bash(npm test)"]},
        "hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "my-own-linter.sh"}]}]}
    });
    std::fs::write(&p, serde_json::to_string_pretty(&user).unwrap()).unwrap();
    p
}

fn count_owned(v: &Value) -> usize {
    v.to_string().matches("--at-owner agenttracker").count()
}

// fake-agent contract test
#[test]
fn test_repeated_connector_install_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let settings = settings_with_user_config(dir.path());
    let hook = dir.path().join("agenttracker-hook.exe");
    std::fs::write(&hook, b"fake").unwrap();
    let c = ClaudeConnector::with_paths(settings.clone(), dir.path().join("state"));
    let r1 = c.install(&hook).unwrap();
    assert!(!r1.added.is_empty());
    let after1: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    let r2 = c.install(&hook).unwrap();
    let r3 = c.install(&hook).unwrap();
    let after3: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    assert_eq!(after1, after3, "re-installing must not change the file");
    assert!(r2.added.is_empty() && r3.added.is_empty());
    assert_eq!(count_owned(&after1), count_owned(&after3));
    assert_eq!(r1.backup_path, r3.backup_path, "backup is taken once, not overwritten");
    assert!(c.detect().hooks_installed);
    assert_eq!(after3["model"], "opus");
}

// fake-agent contract test
#[test]
fn test_uninstall_preserves_unrelated_config() {
    let dir = tempfile::tempdir().unwrap();
    let settings = settings_with_user_config(dir.path());
    let original: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    let hook = dir.path().join("agenttracker-hook.exe");
    std::fs::write(&hook, b"fake").unwrap();
    let c = ClaudeConnector::with_paths(settings.clone(), dir.path().join("state"));

    // Case 1: untouched since install → exact restore.
    c.install(&hook).unwrap();
    let r = c.uninstall().unwrap();
    assert_eq!(r.method, "restored_backup");
    let restored: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    assert_eq!(restored, original);

    // Case 2: the user edited settings after install → surgical removal keeps their edits.
    c.install(&hook).unwrap();
    let mut edited: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    edited["theme"] = json!("dark");
    std::fs::write(&settings, serde_json::to_string_pretty(&edited).unwrap()).unwrap();
    let r = c.uninstall().unwrap();
    assert_eq!(r.method, "surgical_removal");
    let after: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    assert_eq!(count_owned(&after), 0);
    assert_eq!(after["theme"], "dark");
    assert_eq!(after["model"], "opus");
    assert_eq!(after["permissions"], original["permissions"]);
    assert!(after.to_string().contains("my-own-linter.sh"), "user's own hook survives");
    assert!(!c.detect().hooks_installed);
}

// fake-agent contract test
#[test]
fn test_db_migration_v001_to_v002() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(Migrator::run_up_to(&conn, Some("v001")).unwrap(), ["v001"]);
        conn.execute("INSERT INTO tasks(id, title, instruction, definition_version, lifecycle, created_at, updated_at) VALUES ('t1','Old task','x',1,'draft',1,1)", []).unwrap();
        let has_templates: i64 = conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name='check_templates'", [], |r| r.get(0)).unwrap();
        assert_eq!(has_templates, 0);
    }
    // Opening with the current app applies v002+ and keeps existing data.
    let db = Db::open(&path).unwrap();
    let conn = db.conn();
    let versions = Migrator::applied_versions(&conn).unwrap();
    assert!(versions.starts_with(&["v001".to_string(), "v002".to_string()]), "{versions:?}");
    let title: String = conn.query_row("SELECT title FROM tasks WHERE id='t1'", [], |r| r.get(0)).unwrap();
    assert_eq!(title, "Old task");
    conn.execute("INSERT INTO check_templates(id, name, check_type, params, created_at) VALUES ('c1','n','originals_unchanged','{}',1)", []).unwrap();
    // Re-running is a no-op.
    assert!(Migrator::run(&conn).unwrap().is_empty());
}

// fake-agent contract test
#[test]
fn test_queue_enqueue_drain_deliver() {
    let fx = Fixture::new();
    let q = Queue::new(fx.db.shared());
    let a = q.enqueue("claude_code", &json!({"n": 1})).unwrap();
    let b = q.enqueue("claude_code", &json!({"n": 2})).unwrap();
    assert_eq!(q.pending_count().unwrap(), 2);
    let drained = q.drain_pending().unwrap();
    assert_eq!(drained.iter().map(|e| e.payload["n"].as_i64().unwrap()).collect::<Vec<_>>(), [1, 2], "FIFO");
    assert!(q.mark_delivered(&a).unwrap());
    assert_eq!(q.pending_count().unwrap(), 1);
    assert_eq!(q.drain_pending().unwrap()[0].id, b);
    assert!(q.mark_delivered(&b).unwrap());
    assert_eq!(q.pending_count().unwrap(), 0);

    // Offline fallback file written by the hook is imported once, then emptied.
    let fallback = fx.dir.path().join("fallback_queue.jsonl");
    let line = claude_envelope("PreToolUse", 9, json!({"session_id": "s-q", "tool_name": "Read", "tool_input": {}}));
    std::fs::write(&fallback, format!("{}\n{}\nnot json\n", line, line)).unwrap();
    let imported = fx.collector.queue.import_fallback_file(&fallback).unwrap();
    assert!(imported >= 2);
    fx.collector.process_pending().unwrap();
    assert_eq!(fx.engine.events.by_session("s-q").unwrap().len(), 1, "replayed duplicates dedupe");
    assert_eq!(fx.collector.queue.import_fallback_file(&fallback).unwrap(), 0, "not imported twice");
}

// fake-agent contract test
#[test]
fn test_hash_chain_append_and_verify() {
    let fx = Fixture::new();
    for i in 0..5 {
        fx.deliver(claude_envelope("PreToolUse", 100 + i, json!({"session_id": "s-chain", "tool_name": format!("t{i}"), "tool_input": {}})));
    }
    let chain: ChainStore = fx.engine.events.chain().unwrap();
    assert_eq!(chain.len().unwrap(), 5);
    let ok = chain.verify_range(1, 5).unwrap();
    assert!(ok.ok, "{}", ok.detail);
    assert_eq!(ok.checked, 5);

    // Tamper with one stored link → verification pinpoints it.
    fx.db.conn().execute("UPDATE chain_entries SET event_hash = ?1 WHERE seq = 3", ["07".repeat(32)]).unwrap();
    let bad = chain.verify_range(1, 5).unwrap();
    assert!(!bad.ok);
    assert_eq!(bad.first_bad_seq, Some(3));
}


// =================================================================================================
// Phase 3 additions. Still fake-agent contract tests: no live Claude Code / Cursor / Copilot.
// =================================================================================================

/// A second, independent workspace next to the fixture's default one.
fn named_workspace(fx: &Fixture, name: &str, inputs: &[(&str, &str)]) -> std::path::PathBuf {
    let work = fx.dir.path().join(name);
    std::fs::create_dir_all(work.join("input")).unwrap();
    std::fs::create_dir_all(work.join("output")).unwrap();
    for (n, c) in inputs {
        std::fs::write(work.join("input").join(n), c).unwrap();
    }
    work
}

fn task_in(fx: &Fixture, work: &Path, title: &str) -> agenttracker_core::task::TaskDefinition {
    use agenttracker_core::task::{FolderRole, NewTask, TaskFolder};
    let s = |p: std::path::PathBuf| p.to_string_lossy().into_owned();
    fx.engine
        .tasks
        .create(NewTask {
            title: title.into(),
            instruction: "Copy input to output".into(),
            connector: "claude_code".into(),
            folders: vec![
                TaskFolder { folder_path: s(work.to_path_buf()), role: FolderRole::Working },
                TaskFolder { folder_path: s(work.join("input")), role: FolderRole::Protected },
                TaskFolder { folder_path: s(work.join("output")), role: FolderRole::AllowedWrite },
            ],
            checks: vec![
                check("originals_unchanged", json!({}), true),
                check("inputs_preserved_in_output", json!({"input": "input", "output": "output"}), true),
                check("changes_within_scope", json!({}), true),
            ],
        })
        .unwrap()
}

// fake-agent contract test
#[test]
fn test_watcher_interruption_gap_recorded() {
    let fx = Fixture::new();
    let work = fx.workspace(&[("a.txt", "alpha")]);
    let task = fx.copy_task(&work, &["a.txt"], None);
    fx.engine.capture_baseline(&task.id).unwrap();
    fx.engine.start(&task.id).unwrap();

    // Simulate an app restart: a fresh engine over the same database has no live watchers.
    let restarted = agenttracker_core::engine::Engine::new(fx.db.clone(), vec![]);
    assert!(restarted.live_view(&task.id).gaps.is_empty());
    let resumed = restarted.resume_running().unwrap();
    assert_eq!(resumed, vec![task.id.clone()]);
    let view = restarted.live_view(&task.id);
    assert_eq!(view.gaps.len(), 1, "{:?}", view.gaps);
    assert!(view.gaps[0].contains("restarted"), "gap explains what happened: {}", view.gaps[0]);
    assert!(view.observing, "live watching resumed");

    // Explicit gaps (e.g. a watcher that couldn't start) are listed too.
    restarted.add_gap(&task.id, "Live watching couldn't start for X".into());
    assert_eq!(restarted.live_view(&task.id).gaps.len(), 2);

    // Changes made "while closed" are still caught by the final snapshot comparison.
    copy_all(&work);
    let run = restarted.verify(&task.id).unwrap();
    assert_eq!(run.overall_verdict, Some(CheckStatus::Passed), "{:#?}", run.results);
    // Resuming again is a no-op once the task is finished.
    assert!(restarted.resume_running().unwrap().is_empty());
}

// fake-agent contract test
#[test]
fn test_manual_edits_mixed_with_agent_activity() {
    let fx = Fixture::new();
    let work = fx.workspace(&[("a.txt", "alpha"), ("b.txt", "beta")]);
    let task = fx.copy_task(&work, &["a.txt", "b.txt"], None);
    fx.engine.capture_baseline(&task.id).unwrap();
    fx.engine.start(&task.id).unwrap();
    let cwd = work.to_string_lossy().to_string();

    // Agent copies one file...
    fx.deliver(claude_envelope("PreToolUse", 1_000, json!({"session_id": "s-mix", "cwd": cwd, "tool_name": "Write", "tool_input": {"file_path": "output/a.txt"}})));
    std::fs::copy(work.join("input/a.txt"), work.join("output/a.txt")).unwrap();
    fx.deliver(claude_envelope("PostToolUse", 2_000, json!({"session_id": "s-mix", "cwd": cwd, "tool_name": "Write", "tool_response": {"ok": true}})));
    // ...the person copies the other one by hand and adds a note (no hook event for either).
    std::fs::copy(work.join("input/b.txt"), work.join("output/b.txt")).unwrap();
    std::fs::write(work.join("output/my-notes.txt"), "written by a human").unwrap();
    fx.deliver(claude_envelope("Stop", 3_000, json!({"session_id": "s-mix", "cwd": cwd})));

    let run = fx.engine.verify(&task.id).unwrap();
    // File checks judge the folder, whoever made the change.
    assert_eq!(run.overall_verdict, Some(CheckStatus::Passed), "{:#?}", run.results);
    // Only what the agent reported is recorded as agent activity — manual edits are never attributed to it.
    let evs = fx.engine.events.by_task(&task.id).unwrap();
    assert_eq!(evs.len(), 3);
    assert!(evs.iter().all(|e| e.capture_source == CaptureSource::Hook));
    assert!(!serde_json::to_string(&evs).unwrap().contains("my-notes.txt"));

    // A manual edit outside the allowed folder is still caught.
    let task2 = fx.copy_task(&work, &[], None);
    fx.engine.capture_baseline(&task2.id).unwrap();
    std::fs::write(work.join("stray.txt"), "oops").unwrap();
    let run2 = fx.engine.verify(&task2.id).unwrap();
    assert_eq!(status_of(&run2, "changes_within_scope"), CheckStatus::Failed, "{:#?}", run2.results);
}

// fake-agent contract test
#[test]
fn test_concurrent_sessions() {
    let fx = Fixture::new();
    let w1 = named_workspace(&fx, "job-one", &[("a.txt", "one")]);
    let w2 = named_workspace(&fx, "job-two", &[("b.txt", "two")]);
    let t1 = task_in(&fx, &w1, "Job one");
    let t2 = task_in(&fx, &w2, "Job two");
    for t in [&t1, &t2] {
        fx.engine.capture_baseline(&t.id).unwrap();
        fx.engine.start(&t.id).unwrap();
    }
    let (c1, c2) = (w1.to_string_lossy().to_string(), w2.to_string_lossy().to_string());
    // Interleaved deliveries from two agent sessions working at the same time.
    let mut ts = 0;
    let mut next = || {
        ts += 1_000;
        ts
    };
    fx.deliver(claude_envelope("SessionStart", next(), json!({"session_id": "s-one", "cwd": c1})));
    fx.deliver(claude_envelope("SessionStart", next(), json!({"session_id": "s-two", "cwd": c2})));
    fx.deliver(claude_envelope("PreToolUse", next(), json!({"session_id": "s-two", "cwd": c2, "tool_name": "Bash", "tool_input": {"command": "cp"}})));
    fx.deliver(claude_envelope("PreToolUse", next(), json!({"session_id": "s-one", "cwd": c1, "tool_name": "Bash", "tool_input": {"command": "cp"}})));
    copy_all(&w1);
    copy_all(&w2);
    fx.deliver(claude_envelope("Stop", next(), json!({"session_id": "s-one", "cwd": c1})));
    fx.deliver(claude_envelope("Stop", next(), json!({"session_id": "s-two", "cwd": c2})));

    assert_eq!(fx.engine.tasks.get(&t1.id).unwrap().session_id.as_deref(), Some("s-one"));
    assert_eq!(fx.engine.tasks.get(&t2.id).unwrap().session_id.as_deref(), Some("s-two"));
    let e1 = fx.engine.events.by_task(&t1.id).unwrap();
    let e2 = fx.engine.events.by_task(&t2.id).unwrap();
    assert_eq!(e1.len(), 3);
    assert_eq!(e2.len(), 3);
    assert!(e1.iter().all(|e| e.session_id.as_deref() == Some("s-one")));
    assert!(e2.iter().all(|e| e.session_id.as_deref() == Some("s-two")));
    for t in [&t1, &t2] {
        let run = fx.engine.verify(&t.id).unwrap();
        assert_eq!(run.overall_verdict, Some(CheckStatus::Passed), "{:#?}", run.results);
    }
}

// fake-agent contract test
#[test]
fn test_collector_outage_recovery() {
    let fx = Fixture::new();
    // While the collector was down, the hook appended envelopes to its fallback file.
    let fallback = fx.dir.path().join("fallback_queue.jsonl");
    let lines: Vec<String> = (1..=3)
        .map(|i| claude_envelope("PreToolUse", i * 1_000, json!({"session_id": "s-out", "tool_name": format!("t{i}"), "tool_input": {}})).to_string())
        .collect();
    std::fs::write(&fallback, format!("{}\nnot json at all\n", lines.join("\n"))).unwrap();

    let imported = fx.collector.queue.import_fallback_file(&fallback).unwrap();
    assert_eq!(imported, 3, "malformed lines are skipped, good ones kept");
    assert!(!fallback.exists(), "file is consumed after import");
    fx.collector.process_pending().unwrap();
    assert_eq!(fx.engine.events.by_session("s-out").unwrap().len(), 3);
    assert_eq!(fx.collector.queue.pending_count().unwrap(), 0);

    // The same deliveries arriving again (e.g. replay after a crash mid-import) don't duplicate.
    std::fs::write(&fallback, lines.join("\n")).unwrap();
    fx.collector.queue.import_fallback_file(&fallback).unwrap();
    fx.collector.process_pending().unwrap();
    assert_eq!(fx.engine.events.by_session("s-out").unwrap().len(), 3);
    // Importing when there's no file is fine.
    assert_eq!(fx.collector.queue.import_fallback_file(&fallback).unwrap(), 0);
}

fn hook_binary() -> Option<std::path::PathBuf> {
    let name = if cfg!(windows) { "agenttracker-hook.exe" } else { "agenttracker-hook" };
    let target = std::env::var_os("CARGO_TARGET_DIR").map(std::path::PathBuf::from).unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../target"));
    let p = target.join("debug").join(name);
    p.exists().then_some(p)
}

// fake-agent contract test (runs the real hook binary, with no collector listening)
#[test]
fn test_hook_exits_zero_when_collector_down() {
    let Some(bin) = hook_binary() else {
        eprintln!("SKIPPED: build agenttracker-hook first (cargo build -p agenttracker-hook)");
        return;
    };
    let home = tempfile::tempdir().unwrap();
    // Point the hook at a port where nothing is listening.
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = dead.local_addr().unwrap().port();
    drop(dead);
    std::fs::write(home.path().join("collector.json"), json!({"port": port, "secret": "x"}).to_string()).unwrap();

    let start = std::time::Instant::now();
    let out = std::process::Command::new(&bin)
        .args(["--connector", "claude_code", "--event", "Stop", "--payload", r#"{"session_id":"s-down","password":"hunter2-very-secret"}"#, "--no-stdin"])
        .env("AGENTTRACKER_HOME", home.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "hook must never block the agent");
    assert!(out.stdout.is_empty(), "no stdout (agents may read it as a decision)");
    assert!(start.elapsed() < std::time::Duration::from_secs(5), "took {:?}", start.elapsed());
    let saved = std::fs::read_to_string(home.path().join("fallback_queue.jsonl")).expect("delivery kept in the fallback file");
    assert!(saved.contains("s-down"));
    assert!(!saved.contains("hunter2-very-secret"), "fallback file is redacted");

    // And with no collector.json at all.
    let home2 = tempfile::tempdir().unwrap();
    let out2 = std::process::Command::new(&bin).args(["--connector", "cursor", "--event", "stop", "--no-stdin"]).env("AGENTTRACKER_HOME", home2.path()).output().unwrap();
    assert_eq!(out2.status.code(), Some(0));
    assert!(home2.path().join("fallback_queue.jsonl").exists());
}

// fake-agent contract test
#[test]
fn test_verify_now_dedupe() {
    let fx = Fixture::new();
    let work = fx.workspace(&[("a.txt", "alpha")]);
    let task = fx.copy_task(&work, &["a.txt"], None);
    fx.engine.capture_baseline(&task.id).unwrap();
    fx.engine.start(&task.id).unwrap();
    copy_all(&work);

    let first = fx.engine.verify_with_key(&task.id, Some("click-1")).unwrap();
    assert!(!first.deduplicated);
    // Double click: same key → same run, nothing re-run.
    let again = fx.engine.verify_with_key(&task.id, Some("click-1")).unwrap();
    assert!(again.deduplicated);
    assert_eq!(again.run.id, first.run.id);
    assert_eq!(fx.engine.results.runs_for_task(&task.id).unwrap().len(), 1);
    // A deliberate re-run (new key) verifies again.
    let rerun = fx.engine.verify_with_key(&task.id, Some("click-2")).unwrap();
    assert!(!rerun.deduplicated);
    assert_ne!(rerun.run.id, first.run.id);
    // The same key on a different task is not confused with this one.
    let other = fx.copy_task(&work, &[], Some(vec![check("originals_unchanged", json!({}), true)]));
    fx.engine.capture_baseline(&other.id).unwrap();
    let o = fx.engine.verify_with_key(&other.id, Some("click-1")).unwrap();
    assert!(!o.deduplicated);
    assert_eq!(o.run.task_id, other.id);
}

// fake-agent contract test
#[test]
fn test_frozen_definition_used_during_verify() {
    let fx = Fixture::new();
    let work = fx.workspace(&[("a.txt", "alpha")]);
    let task = fx.copy_task(&work, &[], Some(vec![check("originals_unchanged", json!({}), true)]));
    fx.engine.capture_baseline(&task.id).unwrap();
    let run1 = fx.engine.verify(&task.id).unwrap();
    assert_eq!(run1.results.len(), 1);
    let v1 = run1.definition_version;

    // The user edits the checks afterwards (adds a check that will fail).
    let edited = fx
        .engine
        .tasks
        .update_checks(&task.id, vec![check("originals_unchanged", json!({}), true), check("required_outputs_exist", json!({"paths": ["output/missing.txt"]}), true)])
        .unwrap();
    assert_eq!(edited.definition_version, v1 + 1);

    // The earlier run's definition is frozen and unchanged.
    let frozen = fx.engine.tasks.snapshot(&task.id, v1).unwrap().expect("frozen snapshot");
    assert_eq!(frozen.checks.len(), 1);
    let stored = fx.engine.results.get_run(&run1.id).unwrap().expect("stored run");
    assert_eq!(stored.definition_version, v1);
    assert_eq!(stored.overall_verdict, Some(CheckStatus::Passed));

    // A new verification uses (and freezes) the new definition.
    let run2 = fx.engine.verify(&task.id).unwrap();
    assert_eq!(run2.definition_version, v1 + 1);
    assert_eq!(run2.results.len(), 2);
    assert_eq!(run2.overall_verdict, Some(CheckStatus::Failed));
    assert_eq!(fx.engine.tasks.snapshot(&task.id, v1 + 1).unwrap().unwrap().checks.len(), 2);

    // Checks can't be edited while a verification is in progress.
    let t3 = fx.copy_task(&work, &[], None);
    fx.engine.capture_baseline(&t3.id).unwrap();
    fx.engine.tasks.transition(&t3.id, agenttracker_core::task::Lifecycle::Verifying).unwrap();
    assert!(fx.engine.tasks.update_checks(&t3.id, vec![]).is_err());
}

// fake-agent contract test (in-process HTTP, no network)
#[tokio::test]
async fn test_unauthorized_requests_rejected() {
    use agenttracker_core::server::{build_router, AppState};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let fx = Fixture::new();
    let port = 45555;
    let secret = "test-secret-value".to_string();
    let state = AppState::new(
        fx.engine.clone(),
        fx.collector.clone(),
        port,
        Some("launch-token-123".into()),
        secret.clone(),
        None,
        fx.dir.path().to_path_buf(),
        fx.dir.path().join("home"),
    );
    let app = build_router(state);
    let host = format!("127.0.0.1:{port}");
    let send = |req: Request<Body>| {
        let app = app.clone();
        async move { app.oneshot(req).await.unwrap().status() }
    };
    let get = |path: &str, host: &str| Request::get(path).header("host", host).body(Body::empty()).unwrap();

    // UI API without a session cookie.
    assert_eq!(send(get("/api/ui/tasks", &host)).await, StatusCode::UNAUTHORIZED);
    assert_eq!(send(get("/api/ui/settings", &host)).await, StatusCode::UNAUTHORIZED);
    let post_ui = Request::post("/api/ui/tasks").header("host", &host).header("x-requested-with", "agenttracker").header("content-type", "application/json").body(Body::from("{}")).unwrap();
    assert_eq!(send(post_ui).await, StatusCode::UNAUTHORIZED);
    // A forged cookie doesn't help.
    let forged = Request::get("/api/ui/tasks").header("host", &host).header("cookie", "at_session=not-a-real-session").body(Body::empty()).unwrap();
    assert_eq!(send(forged).await, StatusCode::UNAUTHORIZED);
    // DNS-rebinding style host is refused before anything else.
    assert_eq!(send(get("/api/ui/tasks", "evil.example:80")).await, StatusCode::FORBIDDEN);
    assert_eq!(send(get("/", "evil.example")).await, StatusCode::FORBIDDEN);
    // Wrong launch token.
    assert_eq!(send(get("/launch?token=wrong", &host)).await, StatusCode::FORBIDDEN);

    // Hook ingest without / with a bad signature.
    let body = claude_envelope("Stop", 1, json!({"session_id": "s-auth"})).to_string();
    let unsigned = Request::post("/ingest").header("host", &host).body(Body::from(body.clone())).unwrap();
    assert_eq!(send(unsigned).await, StatusCode::UNAUTHORIZED);
    let ts = (agenttracker_core::now_ms() / 1000).to_string();
    let bad = Request::post("/ingest")
        .header("host", &host)
        .header("x-at-timestamp", &ts)
        .header("x-at-signature", tracker_common::security::sign_body("wrong-secret", &ts, body.as_bytes()))
        .body(Body::from(body.clone()))
        .unwrap();
    assert_eq!(send(bad).await, StatusCode::UNAUTHORIZED);
    let stale_ts = (agenttracker_core::now_ms() / 1000 - 3600).to_string();
    let stale = Request::post("/ingest")
        .header("host", &host)
        .header("x-at-timestamp", &stale_ts)
        .header("x-at-signature", tracker_common::security::sign_body(&secret, &stale_ts, body.as_bytes()))
        .body(Body::from(body.clone()))
        .unwrap();
    assert_eq!(send(stale).await, StatusCode::UNAUTHORIZED, "old signatures can't be replayed");
    assert_eq!(fx.collector.queue.pending_count().unwrap(), 0, "nothing rejected was queued");

    // Control: a correctly signed delivery is accepted, and a real launch gives a working session.
    let good = Request::post("/ingest")
        .header("host", &host)
        .header("x-at-timestamp", &ts)
        .header("x-at-signature", tracker_common::security::sign_body(&secret, &ts, body.as_bytes()))
        .body(Body::from(body))
        .unwrap();
    assert_eq!(send(good).await, StatusCode::ACCEPTED);
    let launch = app.clone().oneshot(get("/launch?token=launch-token-123", &host)).await.unwrap();
    assert_eq!(launch.status(), StatusCode::FOUND);
    let cookie = launch.headers().get("set-cookie").unwrap().to_str().unwrap().split(';').next().unwrap().to_string();
    let authed = Request::get("/api/ui/tasks").header("host", &host).header("cookie", &cookie).body(Body::empty()).unwrap();
    assert_eq!(send(authed).await, StatusCode::OK);
    // Non-GET with a session but without the CSRF header is refused.
    let no_csrf = Request::post("/api/ui/pause").header("host", &host).header("cookie", &cookie).body(Body::empty()).unwrap();
    assert_eq!(send(no_csrf).await, StatusCode::FORBIDDEN);
    // The launch token is single-use.
    assert_eq!(send(get("/launch?token=launch-token-123", &host)).await, StatusCode::FORBIDDEN);
}

fn copilot_paths(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let hooks_dir = dir.join(".copilot/hooks");
    std::fs::create_dir_all(&hooks_dir).unwrap();
    // The user's own, unrelated hook file in the same folder.
    std::fs::write(hooks_dir.join("my-team.json"), r#"{"version":1,"hooks":{"sessionStart":[{"type":"command","bash":"./team-audit.sh"}]}}"#).unwrap();
    (hooks_dir.join("agenttracker.json"), hooks_dir.join("my-team.json"))
}

// fake-agent contract test
#[test]
fn test_copilot_install_idempotent() {
    use agenttracker_core::connectors::copilot::{CopilotConnector, HOOK_EVENTS};
    let dir = tempfile::tempdir().unwrap();
    let (hooks, _) = copilot_paths(dir.path());
    let bin = dir.path().join("agenttracker-hook.exe");
    std::fs::write(&bin, b"fake").unwrap();
    let c = CopilotConnector::with_paths(hooks.clone(), dir.path().join("state"));
    assert!(!c.detect().hooks_installed);

    let preview = c.preview(&bin).unwrap();
    assert!(preview.changes && !preview.file_exists);
    assert!(!hooks.exists(), "preview never writes");

    let r1 = c.install(&bin).unwrap();
    assert_eq!(r1.added.len(), HOOK_EVENTS.len());
    let after1: Value = serde_json::from_str(&std::fs::read_to_string(&hooks).unwrap()).unwrap();
    assert_eq!(after1["version"], 1);
    for e in HOOK_EVENTS {
        let entries = after1["hooks"][*e].as_array().unwrap_or_else(|| panic!("{e} registered"));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["type"], "command");
        assert!(entries[0]["bash"].as_str().unwrap().contains("--at-owner agenttracker"));
        assert!(entries[0]["powershell"].as_str().unwrap().starts_with("& "));
    }
    // Decision hooks that could block the agent are never registered.
    assert!(after1["hooks"].get("preToolUse").is_none() && after1["hooks"].get("permissionRequest").is_none());

    let r2 = c.install(&bin).unwrap();
    let r3 = c.install(&bin).unwrap();
    let after3: Value = serde_json::from_str(&std::fs::read_to_string(&hooks).unwrap()).unwrap();
    assert_eq!(after1, after3, "re-installing must not change the file");
    assert!(r2.added.is_empty() && r3.added.is_empty() && r3.updated.is_empty());
    assert!(!c.preview(&bin).unwrap().changes);
    assert!(c.detect().hooks_installed);
    assert!(c.connection_test().steps.iter().any(|s| s.name == "AgentTracker hooks" && s.ok));
}

// fake-agent contract test
#[test]
fn test_copilot_uninstall_preserves_unrelated() {
    use agenttracker_core::connectors::copilot::CopilotConnector;
    let dir = tempfile::tempdir().unwrap();
    let (hooks, team) = copilot_paths(dir.path());
    let team_before = std::fs::read(&team).unwrap();
    let bin = dir.path().join("agenttracker-hook.exe");
    std::fs::write(&bin, b"fake").unwrap();
    let c = CopilotConnector::with_paths(hooks.clone(), dir.path().join("state"));

    // Case 1: untouched since install → AgentTracker's file goes away entirely (or is left with none of its hooks).
    c.install(&bin).unwrap();
    c.uninstall().unwrap();
    if hooks.exists() {
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&hooks).unwrap()).unwrap();
        assert_eq!(count_owned(&v), 0);
    }
    assert!(!c.detect().hooks_installed);

    // Case 2: the user added their own entry to our file after install → it survives.
    c.install(&bin).unwrap();
    let mut edited: Value = serde_json::from_str(&std::fs::read_to_string(&hooks).unwrap()).unwrap();
    edited["hooks"]["sessionEnd"].as_array_mut().unwrap().push(json!({"type": "command", "bash": "./my-cleanup.sh"}));
    std::fs::write(&hooks, serde_json::to_string_pretty(&edited).unwrap()).unwrap();
    c.uninstall().unwrap();
    let after: Value = serde_json::from_str(&std::fs::read_to_string(&hooks).unwrap()).unwrap();
    assert_eq!(count_owned(&after), 0);
    assert!(after.to_string().contains("my-cleanup.sh"), "user's own hook survives: {after}");
    assert!(!c.detect().hooks_installed);

    // The unrelated hook file in the same folder was never touched.
    assert_eq!(std::fs::read(&team).unwrap(), team_before);
}

fn copilot_env(event: &str, ts_ns: i64, payload: Value) -> agenttracker_core::connectors::HookEnvelope {
    agenttracker_core::connectors::HookEnvelope { connector: "copilot_vscode".into(), hook_event: event.into(), hook_ts_ns: ts_ns, hook_version: "test".into(), payload }
}

// fake-agent contract test
#[test]
fn test_copilot_payload_normalization() {
    use agenttracker_core::connectors::normalize;
    let base = |extra: Value| {
        let mut p = json!({"sessionId": "cp-1", "timestamp": 1_762_000_000_000_i64, "cwd": "C:/work"});
        for (k, v) in extra.as_object().unwrap() {
            p[k] = v.clone();
        }
        p
    };
    let cases = [
        ("sessionStart", base(json!({"source": "new"})), EventType::SessionStarted),
        ("userPromptSubmitted", base(json!({"prompt": "sort my photos"})), EventType::TaskSubmitted),
        ("postToolUse", base(json!({"toolName": "editFile", "toolArgs": {"path": "a.txt"}, "toolResult": {"ok": true}})), EventType::ToolFinished),
        ("agentStop", base(json!({"stopReason": "end_turn"})), EventType::AgentReportedCompletion),
        ("sessionEnd", base(json!({"reason": "complete"})), EventType::SessionEnded),
    ];
    for (i, (name, payload, want)) in cases.into_iter().enumerate() {
        let ev = normalize(&copilot_env(name, (i as i64 + 1) * 1_000, payload)).unwrap();
        assert_eq!(ev.event_type, want, "{name}");
        assert_eq!(ev.session_id.as_deref(), Some("cp-1"), "{name}");
        assert_eq!(ev.working_dir.as_deref(), Some("C:/work"), "{name}");
        assert_eq!(ev.source_ts, Some(1_762_000_000_000), "{name}: uses the agent's timestamp");
    }
    let tool = normalize(&copilot_env("postToolUse", 9, base(json!({"toolName": "editFile", "toolArgs": {"path": "a.txt"}})))).unwrap();
    assert_eq!(tool.payload_redacted["tool"], "editFile");

    // PascalCase alias with the snake_case payload shape.
    let snake = normalize(&copilot_env("PostToolUse", 10, json!({"hook_event_name": "PostToolUse", "session_id": "cp-2", "timestamp": "2026-11-03T10:00:00Z", "cwd": "/w", "tool_name": "bash", "tool_input": {"command": "ls"}}))).unwrap();
    assert_eq!(snake.event_type, EventType::ToolFinished);
    assert_eq!(snake.session_id.as_deref(), Some("cp-2"));
    assert_eq!(snake.payload_redacted["tool"], "bash");

    // Secrets in prompts are redacted; transcript paths are dropped.
    let secret = format!("ghp_{}", "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789");
    let p = normalize(&copilot_env("userPromptSubmitted", 11, base(json!({"prompt": format!("use token {secret}"), "transcriptPath": "/tmp/t.json"})))).unwrap();
    assert!(!p.payload_redacted.to_string().contains(&secret));
    assert!(!serde_json::to_string(&p).unwrap().contains("/tmp/t.json"));

    // Through the real collector path too.
    let fx = Fixture::new();
    fx.deliver(serde_json::to_value(copilot_env("sessionStart", 20, base(json!({})))).unwrap());
    fx.deliver(serde_json::to_value(copilot_env("agentStop", 21, base(json!({})))).unwrap());
    let stored = fx.engine.events.by_session("cp-1").unwrap();
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[1].event_type, EventType::AgentReportedCompletion);
}

// fake-agent contract test
#[test]
fn test_copilot_unsupported_version() {
    use agenttracker_core::connectors::{compute_state, normalize, ConnectorState, DetectResult, UNSUPPORTED_MARKER};
    // Missing sessionId → unsupported/changed agent version, recorded as a capture error.
    let ev = normalize(&copilot_env("postToolUse", 1, json!({"timestamp": 1, "toolName": "x"}))).unwrap();
    assert_eq!(ev.event_type, EventType::CaptureError);
    assert_eq!(ev.payload_redacted[UNSUPPORTED_MARKER], true);
    // Missing timestamp → same.
    let ev2 = normalize(&copilot_env("agentStop", 2, json!({"sessionId": "cp-x"}))).unwrap();
    assert_eq!(ev2.event_type, EventType::CaptureError);
    assert_eq!(ev2.payload_redacted[UNSUPPORTED_MARKER], true);
    // An unknown event name is a capture error but not flagged as a version problem.
    let ev3 = normalize(&copilot_env("brandNewEvent", 3, json!({"sessionId": "cp-x", "timestamp": 3}))).unwrap();
    assert_eq!(ev3.event_type, EventType::CaptureError);
    assert!(ev3.payload_redacted.get(UNSUPPORTED_MARKER).is_none());

    // The connector state reflects it in plain words.
    let det = DetectResult {
        connector: "copilot_vscode".into(),
        agent_found: true,
        agent_version: Some("1.106.0".into()),
        config_path: "x".into(),
        config_exists: true,
        hooks_installed: true,
        notes: vec![],
    };
    let (state, msg) = compute_state(&det, true, 1, true);
    assert_eq!(state, ConnectorState::UnsupportedVersion);
    assert!(msg.contains("file checks still work"));
    assert_eq!(compute_state(&det, true, 0, false).0, ConnectorState::Connected);

    // Stored through the collector it never breaks ingestion of later good events.
    let fx = Fixture::new();
    fx.deliver(serde_json::to_value(copilot_env("postToolUse", 4, json!({"toolName": "x"}))).unwrap());
    fx.deliver(serde_json::to_value(copilot_env("sessionStart", 5, json!({"sessionId": "cp-ok", "timestamp": 5}))).unwrap());
    assert_eq!(fx.engine.events.count().unwrap(), 2);
    assert_eq!(fx.engine.events.by_session("cp-ok").unwrap().len(), 1);
}
