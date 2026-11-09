//! Orchestrates the task flow: baseline snapshot → (agent works, observed) → verification snapshot
//! → run checks → overall verdict.
use crate::db::Db;
use crate::fs_observer::{ChangeHint, ManifestType, Observer, ObserverHandle, Snapshotter};
use crate::store::{EventStore, ManifestStore, ResultStore, TemplateStore, VerificationRun};
use crate::task::{Lifecycle, TaskDefinition, TaskStore};
use crate::verifiers::{overall_verdict, Evidence, VerifierRegistry};
use crate::{CoreError, Result};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
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
    /// Houndbit's own files (DB, queue, evidence) — never snapshotted.
    pub exclude_paths: Vec<PathBuf>,
    observers: Mutex<HashMap<String, Vec<ObserverHandle>>>,
    hints: Mutex<HashMap<String, Vec<ChangeHint>>>,
    /// Known holes in live observation per task (observer failed to start, app restarted, ...).
    gaps: Mutex<HashMap<String, Vec<String>>>,
    /// Idempotency keys for "Verify now": key → (task_id, run_id, ms).
    verify_keys: Mutex<HashMap<String, (String, String, i64)>>,
    verifying: Mutex<HashSet<String>>,
}

/// How long a repeated idempotency key returns the earlier run instead of re-running.
pub const VERIFY_DEDUPE_MS: i64 = 30_000;

/// Live Task view: what was observed independently, and where observation has holes.
#[derive(Debug, Clone, Serialize)]
pub struct LiveView {
    pub observing: bool,
    pub change_count: usize,
    /// Most recent independently observed file changes (newest last, at most 200).
    pub recent_changes: Vec<ChangeHint>,
    pub gaps: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VerifyOutcome {
    pub run: VerificationRun,
    /// True when this request repeated a recent idempotency key and the earlier run was returned.
    pub deduplicated: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PurgeReport {
    pub tasks_deleted: usize,
    pub events_deleted: usize,
    /// Old tasks kept because they are still active (their evidence is needed).
    pub kept_active: Vec<String>,
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
            gaps: Mutex::new(HashMap::new()),
            verify_keys: Mutex::new(HashMap::new()),
            verifying: Mutex::new(HashSet::new()),
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
        self.start_observers(&t);
        Ok(t)
    }

    fn start_observers(&self, t: &TaskDefinition) {
        let mut handles = vec![];
        for r in t.snapshot_roots() {
            match Observer::start(PathBuf::from(&r)) {
                Ok(h) => handles.push(h),
                Err(e) => self.add_gap(&t.id, format!("Live watching couldn't start for {r} ({e}). The final check still compares full snapshots.")),
            }
        }
        self.observers.lock().unwrap_or_else(|p| p.into_inner()).insert(t.id.clone(), handles);
    }

    pub fn add_gap(&self, task_id: &str, msg: String) {
        self.gaps.lock().unwrap_or_else(|p| p.into_inner()).entry(task_id.to_string()).or_default().push(msg);
    }

    /// After an app restart: restart live watching for tasks that were running and record the
    /// gap (changes made while Houndbit was closed are only seen by the final snapshot).
    pub fn resume_running(&self) -> Result<Vec<String>> {
        let mut resumed = vec![];
        for t in self.tasks.list()? {
            if t.lifecycle == Lifecycle::Running && !self.observers.lock().unwrap_or_else(|p| p.into_inner()).contains_key(&t.id) {
                self.add_gap(&t.id, "Houndbit was restarted while this task was running. Live watching resumed; changes made while it was closed will still be found by the final snapshot comparison.".into());
                self.start_observers(&t);
                resumed.push(t.id.clone());
            }
            if t.lifecycle == Lifecycle::Verifying {
                // A verification interrupted by a restart never finished; allow a fresh one.
                let _ = self.tasks.transition(&t.id, Lifecycle::Error);
            }
        }
        Ok(resumed)
    }

    pub fn live_view(&self, task_id: &str) -> LiveView {
        let change_count = self.live_hint_count(task_id);
        let hints = self.hints.lock().unwrap_or_else(|p| p.into_inner());
        let all = hints.get(task_id).cloned().unwrap_or_default();
        let recent_changes = all[all.len().saturating_sub(200)..].to_vec();
        let mut gaps = self.gaps.lock().unwrap_or_else(|p| p.into_inner()).get(task_id).cloned().unwrap_or_default();
        let obs = self.observers.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(hs) = obs.get(task_id) {
            for h in hs {
                for e in h.errors() {
                    gaps.push(format!("The folder watcher reported a problem for {}: {e}", h.root.display()));
                }
            }
        }
        if change_count >= 100_000 {
            gaps.push("So many changes happened that live watching stopped recording details. The final check is unaffected.".into());
        }
        LiveView { observing: obs.contains_key(task_id), change_count, recent_changes, gaps }
    }

    /// "Verify now" with double-click protection. The same `key` within [`VERIFY_DEDUPE_MS`]
    /// returns the earlier run; a different key (an intentional re-run) verifies again.
    pub fn verify_with_key(&self, task_id: &str, key: Option<&str>) -> Result<VerifyOutcome> {
        let now = crate::now_ms();
        if let Some(k) = key {
            let mut keys = self.verify_keys.lock().unwrap_or_else(|p| p.into_inner());
            keys.retain(|_, (_, _, ts)| now - *ts < VERIFY_DEDUPE_MS);
            if let Some((tid, run_id, _)) = keys.get(k).cloned() {
                if tid == task_id {
                    if let Some(run) = self.results.get_run(&run_id)? {
                        return Ok(VerifyOutcome { run, deduplicated: true });
                    }
                }
            }
        }
        {
            let mut v = self.verifying.lock().unwrap_or_else(|p| p.into_inner());
            if !v.insert(task_id.to_string()) {
                return Err(CoreError::Invalid("This task is already being checked — the result will appear in a moment.".into()));
            }
        }
        let r = self.verify(task_id);
        self.verifying.lock().unwrap_or_else(|p| p.into_inner()).remove(task_id);
        let run = r?;
        if let Some(k) = key {
            self.verify_keys.lock().unwrap_or_else(|p| p.into_inner()).insert(k.to_string(), (task_id.to_string(), run.id.clone(), now));
        }
        Ok(VerifyOutcome { run, deduplicated: false })
    }

    /// Deletes finished tasks (completed / cancelled / error) last updated before `cutoff_ms`,
    /// with their snapshots, results and events. Refuses to touch active tasks.
    pub fn purge_older_than(&self, cutoff_ms: i64) -> Result<PurgeReport> {
        let mut rep = PurgeReport::default();
        for t in self.tasks.list()? {
            if t.updated_at >= cutoff_ms {
                continue;
            }
            if !matches!(t.lifecycle, Lifecycle::Completed | Lifecycle::Cancelled | Lifecycle::Error) {
                rep.kept_active.push(t.title.clone());
                continue;
            }
            self.stop_observers(&t.id);
            rep.events_deleted += self.db.delete_task_data(&t.id)?;
            rep.tasks_deleted += 1;
            self.hints.lock().unwrap_or_else(|p| p.into_inner()).remove(&t.id);
            self.gaps.lock().unwrap_or_else(|p| p.into_inner()).remove(&t.id);
        }
        Ok(rep)
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
            let window = crate::event::task_window(task.created_at, started_at, None, &events);
            let events = crate::event::only_this_task(events, window);
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
