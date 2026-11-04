//! Shared fixtures for the acceptance tests. Everything here is a FAKE agent: hook deliveries are
//! hand-built JSON envelopes in the documented hook formats, and "agent work" is plain file I/O
//! performed by the test. No live Claude Code / Cursor process is involved.
use agenttracker_core::collector::Collector;
use agenttracker_core::db::Db;
use agenttracker_core::engine::Engine;
use agenttracker_core::task::{FolderRole, NewCheck, NewTask, TaskDefinition, TaskFolder};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct Fixture {
    pub dir: tempfile::TempDir,
    pub db: Db,
    pub engine: Arc<Engine>,
    pub collector: Arc<Collector>,
}

impl Fixture {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("agenttracker.db")).expect("open db");
        let engine = Engine::new(db.clone(), vec![dir.path().join("agenttracker.db")]);
        let collector = Collector::new(&db);
        Self { dir, db, engine, collector }
    }

    /// Creates `work/` with `input/` (protected) and `output/` (allowed_write) folders.
    pub fn workspace(&self, inputs: &[(&str, &str)]) -> PathBuf {
        let work = self.dir.path().join("work");
        std::fs::create_dir_all(work.join("input")).unwrap();
        std::fs::create_dir_all(work.join("output")).unwrap();
        for (name, content) in inputs {
            std::fs::write(work.join("input").join(name), content).unwrap();
        }
        work
    }

    pub fn copy_task(&self, work: &Path, outputs: &[&str], checks: Option<Vec<NewCheck>>) -> TaskDefinition {
        let s = |p: PathBuf| p.to_string_lossy().into_owned();
        let checks = checks.unwrap_or_else(|| {
            vec![
                check("originals_unchanged", json!({}), true),
                check("required_outputs_exist", json!({ "paths": outputs.iter().map(|o| format!("output/{o}")).collect::<Vec<_>>() }), true),
                check("inputs_preserved_in_output", json!({"input": "input", "output": "output"}), true),
                check("changes_within_scope", json!({}), true),
            ]
        });
        self.engine
            .tasks
            .create(NewTask {
                title: "Copy files".into(),
                instruction: "Copy input to output".into(),
                connector: "claude_code".into(),
                folders: vec![
                    TaskFolder { folder_path: s(work.to_path_buf()), role: FolderRole::Working },
                    TaskFolder { folder_path: s(work.join("input")), role: FolderRole::Protected },
                    TaskFolder { folder_path: s(work.join("output")), role: FolderRole::AllowedWrite },
                ],
                checks,
            })
            .expect("create task")
    }

    /// Delivers a fake hook envelope through the real collector path (queue → normalize → store).
    pub fn deliver(&self, envelope: Value) {
        self.collector.accept(serde_json::to_string(&envelope).unwrap().as_bytes()).expect("accept");
        self.collector.process_pending().expect("process");
    }
}

impl Default for Fixture {
    fn default() -> Self {
        Self::new()
    }
}

pub fn check(t: &str, params: Value, required: bool) -> NewCheck {
    NewCheck { check_type: t.into(), params, is_required: required }
}

/// A Claude Code hook envelope as the agenttracker-hook binary would send it.
pub fn claude_envelope(hook_event: &str, ts_ns: i64, payload: Value) -> Value {
    json!({"connector": "claude_code", "hook_event": hook_event, "hook_ts_ns": ts_ns, "hook_version": "test", "payload": payload})
}
