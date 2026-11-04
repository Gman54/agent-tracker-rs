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
