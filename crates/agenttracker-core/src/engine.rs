//! Orchestrates the task flow: baseline snapshot → (agent works, observed) → verification snapshot
//! → run checks → overall verdict.
use crate::db::Db;
use crate::fs_observer::{ChangeHint, ManifestType, Observer, ObserverHandle, Snapshotter};
use crate::store::{EventStore, ManifestStore, ResultStore, TemplateStore, VerificationRun};
use crate::task::{Lifecycle, TaskDefinition, TaskStore};
use crate::verifiers::{overall_verdict, Evidence, VerifierRegistry};
use crate::{CoreError, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub struct Engine {
    pub db: Db,
    pub tasks: TaskStore,
    pub events: EventStore,
    pub manifests: ManifestStore,
    pub results: ResultStore,
    pub templates: TemplateStore,
    pub registry: VerifierRegistry,
    pub snapshotter: Snapshotter,
    /// AgentTracker's own files (DB, queue, evidence) — never snapshotted.
    pub exclude_paths: Vec<PathBuf>,
    observers: Mutex<HashMap<String, Vec<ObserverHandle>>>,
    hints: Mutex<HashMap<String, Vec<ChangeHint>>>,
}

impl Engine {
    pub fn new(db: Db, exclude_paths: Vec<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            tasks: TaskStore::new(db.clone()),
            events: EventStore::new(db.clone()),
            manifests: ManifestStore::new(db.clone()),
            results: ResultStore::new(db.clone()),
            templates: TemplateStore::new(db.clone()),
            registry: VerifierRegistry::default(),
            snapshotter: Snapshotter::new(),
            exclude_paths,
            observers: Mutex::new(HashMap::new()),
            hints: Mutex::new(HashMap::new()),
            db,
        })
    }

    fn snapshot_all(&self, task: &TaskDefinition, mt: ManifestType) -> Result<usize> {
        let roots = task.snapshot_roots();
        for root in &roots {
            let m = self.snapshotter.capture_with(Path::new(root), &self.exclude_paths, mt.clone(), Some(&task.id));
            self.manifests.save(&m)?;
        }
        Ok(roots.len())
    }

    /// draft → baselining → ready. Captures the "before" snapshot of every working/protected folder.
    pub fn capture_baseline(&self, task_id: &str) -> Result<TaskDefinition> {
        let t = self.tasks.transition(task_id, Lifecycle::Baselining)?;
        if t.snapshot_roots().is_empty() {
            self.tasks.transition(task_id, Lifecycle::Draft)?;
            return Err(CoreError::Invalid("Add at least one folder before taking the \"before\" snapshot".into()));
        }
        match self.snapshot_all(&t, ManifestType::Baseline) {
            Ok(_) => self.tasks.transition(task_id, Lifecycle::Ready),
            Err(e) => {
                let _ = self.tasks.transition(task_id, Lifecycle::Error);
                Err(e)
            }
        }
    }

    /// ready → running. Starts live change observers (best effort; hints only).
    pub fn start(&self, task_id: &str) -> Result<TaskDefinition> {
        let t = self.tasks.transition(task_id, Lifecycle::Running)?;
        let handles: Vec<ObserverHandle> = t.snapshot_roots().into_iter().filter_map(|r| Observer::start(PathBuf::from(r)).ok()).collect();
        self.observers.lock().unwrap_or_else(|p| p.into_inner()).insert(task_id.to_string(), handles);
        Ok(t)
    }

    fn stop_observers(&self, task_id: &str) {
        if let Some(handles) = self.observers.lock().unwrap_or_else(|p| p.into_inner()).remove(task_id) {
            let mut hints = self.hints.lock().unwrap_or_else(|p| p.into_inner());
            let entry = hints.entry(task_id.to_string()).or_default();
            for h in handles {
                entry.extend(h.drain());
            }
        }
    }

    pub fn live_hint_count(&self, task_id: &str) -> usize {
        let obs = self.observers.lock().unwrap_or_else(|p| p.into_inner());
        let mut n = self.hints.lock().unwrap_or_else(|p| p.into_inner()).get(task_id).map(Vec::len).unwrap_or(0);
        if let Some(hs) = obs.get(task_id) {
            for h in hs {
                let d = h.drain();
                n += d.len();
                self.hints.lock().unwrap_or_else(|p| p.into_inner()).entry(task_id.to_string()).or_default().extend(d);
            }
        }
        n
    }

    pub fn cancel(&self, task_id: &str) -> Result<TaskDefinition> {
        self.stop_observers(task_id);
        self.tasks.transition(task_id, Lifecycle::Cancelled)
    }

    /// → verifying → completed. Freezes the definition, captures the "after" snapshot, runs every
    /// check and stores the run.
    pub fn verify(&self, task_id: &str) -> Result<VerificationRun> {
        let before = self.tasks.get(task_id)?;
        if !before.lifecycle.can_transition(Lifecycle::Verifying) {
            return Err(CoreError::Lifecycle { from: before.lifecycle.as_str().into(), to: "verifying".into() });
        }
        self.stop_observers(task_id);
        self.tasks.transition(task_id, Lifecycle::Verifying)?;
        let version = self.tasks.freeze(task_id)?;
        let task = self.tasks.snapshot(task_id, version)?.unwrap_or(before);
        let started_at = crate::now_ms();
        let run = (|| -> Result<VerificationRun> {
            self.snapshot_all(&task, ManifestType::Verification)?;
            let mut events = self.events.by_task(task_id)?;
            if let Some(sid) = &task.session_id {
                for e in self.events.by_session(sid)? {
                    if !events.iter().any(|x| x.event_id == e.event_id) {
                        events.push(e);
                    }
                }
            }
            let evidence = Evidence {
                baselines: self.manifests.latest_for_task(task_id, ManifestType::Baseline)?,
                verifications: self.manifests.latest_for_task(task_id, ManifestType::Verification)?,
                events,
                change_hints: self.hints.lock().unwrap_or_else(|p| p.into_inner()).remove(task_id).unwrap_or_default(),
                task: Some(task.clone()),
            };
            Ok(self.run_checks(&task, &evidence, started_at))
        })();
        match run {
            Ok(run) => {
                self.results.save_run(&run)?;
                self.tasks.transition(task_id, Lifecycle::Completed)?;
                Ok(run)
            }
            Err(e) => {
                let _ = self.tasks.transition(task_id, Lifecycle::Error);
                Err(e)
            }
        }
    }

    /// Pure check execution (also used by tests with hand-built evidence).
    pub fn run_checks(&self, task: &TaskDefinition, evidence: &Evidence, started_at: i64) -> VerificationRun {
        let results: Vec<_> = task.checks.iter().map(|c| self.registry.run(&c.id, &c.check_type, &c.params, evidence)).collect();
        let verdict = overall_verdict(&task.checks.iter().zip(&results).map(|(c, r)| (c.is_required, r.status)).collect::<Vec<_>>());
        VerificationRun {
            id: uuid::Uuid::new_v4().to_string(),
            task_id: task.id.clone(),
            definition_version: task.definition_version,
            started_at,
            finished_at: Some(crate::now_ms()),
            overall_verdict: Some(verdict),
            results,
        }
    }
}
