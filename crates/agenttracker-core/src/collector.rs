//! Event collector: accepts hook deliveries into the durable queue, then normalizes, attributes
//! and stores them. Also owns the discovery file the hook binary reads (port + HMAC secret).
use crate::connectors::{self, HookEnvelope};
use crate::event::{AgentEvent, CaptureSource, EventType};
use crate::queue::Queue;
use crate::store::{EventStore, HealthStore};
use crate::task::TaskStore;
use crate::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub const DISCOVERY_FILE: &str = "collector.json";
pub const FALLBACK_FILE: &str = "fallback_queue.jsonl";

/// Written to `~/.agenttracker/collector.json` while the app runs. The secret lets the hook sign
/// requests; the file lives in the user's profile (same trust boundary as the agent itself).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CollectorDiscovery {
    pub port: u16,
    pub secret: String,
    pub pid: u32,
    pub started_at: i64,
}

impl CollectorDiscovery {
    pub fn write(&self, home: &Path) -> Result<PathBuf> {
        std::fs::create_dir_all(home)?;
        let p = home.join(DISCOVERY_FILE);
        let tmp = home.join(format!("{DISCOVERY_FILE}.tmp"));
        std::fs::write(&tmp, serde_json::to_string(self)?)?;
        std::fs::rename(&tmp, &p)?;
        Ok(p)
    }

    pub fn read(home: &Path) -> Option<Self> {
        std::fs::read_to_string(home.join(DISCOVERY_FILE)).ok().and_then(|s| serde_json::from_str(&s).ok())
    }

    pub fn remove(home: &Path) {
        let _ = std::fs::remove_file(home.join(DISCOVERY_FILE));
    }
}

pub struct Collector {
    pub queue: Queue,
    pub events: EventStore,
    pub tasks: TaskStore,
    pub health: HealthStore,
    paused: AtomicBool,
    /// Extra user-supplied words/phrases to scrub (on top of the built-in secret redaction,
    /// which is always on).
    custom_redactions: Mutex<Vec<String>>,
    /// Set by the app so agent prompts can start and finish automatic tasks.
    engine: Mutex<Option<Arc<crate::engine::Engine>>>,
    db: crate::db::Db,
}

/// Replaces each literal phrase (case-sensitive, at least 3 characters) in every string value.
pub fn redact_custom(v: &mut Value, phrases: &[String]) {
    match v {
        Value::String(s) => {
            for p in phrases.iter().filter(|p| p.chars().count() >= 3) {
                if s.contains(p.as_str()) {
                    *s = s.replace(p.as_str(), "[REDACTED:custom]");
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| redact_custom(x, phrases)),
        Value::Object(o) => o.values_mut().for_each(|x| redact_custom(x, phrases)),
        _ => {}
    }
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct ProcessStats {
    pub processed: usize,
    pub stored: usize,
    pub duplicates: usize,
    pub skipped: usize,
}

impl Collector {
    pub fn new(db: &crate::db::Db) -> Arc<Self> {
        Arc::new(Self {
            queue: Queue::new(db.shared()),
            events: EventStore::new(db.clone()),
            tasks: TaskStore::new(db.clone()),
            health: HealthStore::new(db.clone()),
            paused: AtomicBool::new(false),
            custom_redactions: Mutex::new(
                db.get_setting("redaction_patterns").ok().flatten().map(|s| s.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect()).unwrap_or_default(),
            ),
            engine: Mutex::new(None),
            db: db.clone(),
        })
    }

    /// Lets the collector create and check automatic tasks.
    pub fn set_engine(&self, e: Arc<crate::engine::Engine>) {
        *self.engine.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
    }

    fn engine(&self) -> Option<Arc<crate::engine::Engine>> {
        self.engine.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn auto_tasks_on(&self) -> bool {
        self.db.get_setting("auto_tasks").ok().flatten().map(|v| v != "0" && v != "false").unwrap_or(true)
    }

    /// Starts an automatic task for an agent job that no running task covers.
    fn maybe_auto_start(&self, ev: &AgentEvent, queue_id: &str) {
        let starts_job = match ev.event_type {
            EventType::TaskSubmitted => true,
            // Cursor never reports the prompt, so its first event in a folder starts the job.
            EventType::SessionStarted | EventType::ToolStarted | EventType::ToolFinished => ev.connector_name == "cursor",
            _ => false,
        };
        if !starts_job || ev.task_id.is_some() {
            return;
        }
        let Some(wd) = ev.working_dir.clone() else { return };
        let Some(engine) = self.engine() else { return };
        if !self.auto_tasks_on() {
            return;
        }
        if let Ok(Some(_)) = self.tasks.running_task_for_dir(&wd) {
            return;
        }
        if !safe_auto_folder(&wd) {
            return;
        }
        if let Err(e) = auto_start(&engine, ev, &wd, queue_id) {
            eprintln!("automatic task couldn't start: {e}");
        }
    }

    /// Checks an automatic task when its agent says the job is done.
    fn maybe_auto_finish(&self, ev: &AgentEvent) {
        if !matches!(ev.event_type, EventType::AgentReportedCompletion | EventType::SessionEnded) {
            return;
        }
        let Some(tid) = ev.task_id.clone() else { return };
        let Some(engine) = self.engine() else { return };
        let Ok(t) = engine.tasks.get(&tid) else { return };
        if !t.is_auto() || t.lifecycle != crate::task::Lifecycle::Running {
            return;
        }
        if let Err(e) = auto_finish(&engine, &t) {
            eprintln!("automatic check failed: {e}");
        }
    }

    pub fn set_paused(&self, p: bool) {
        self.paused.store(p, Ordering::SeqCst);
    }

    pub fn set_custom_redactions(&self, phrases: Vec<String>) {
        *self.custom_redactions.lock().unwrap_or_else(|p| p.into_inner()) = phrases;
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    /// Accepts one raw delivery (already authenticated). Always enqueues — even while paused —
    /// so nothing is lost; processing resumes when collection is unpaused.
    pub fn accept(&self, body: &[u8]) -> Result<String> {
        let received_at = crate::now_ms();
        let v: Value = serde_json::from_slice(body).unwrap_or_else(|e| json!({"connector": "unknown", "malformed": e.to_string()}));
        let connector = v.get("connector").and_then(Value::as_str).unwrap_or("unknown").to_string();
        // A file's "before" state only exists right now, before the agent's edit runs.
        if !self.is_paused() {
            crate::gitwork::capture::pre_snapshot(&self.db, &v);
        }
        let id = self.queue.enqueue(&connector, &v)?;
        if !self.is_paused() {
            self.quick_list_on_request(&v, &id, received_at);
        }
        Ok(id)
    }

    /// When a prompt for an automatic task arrives, list the working folder right away (names
    /// only, no hashing), before the slower full "before" snapshot. Never fails the delivery.
    fn quick_list_on_request(&self, v: &Value, queue_id: &str, received_at: i64) {
        let Ok(env) = serde_json::from_value::<HookEnvelope>(v.clone()) else { return };
        let Some(ev) = connectors::normalize(&env) else { return };
        if ev.event_type != EventType::TaskSubmitted {
            return;
        }
        let Some(wd) = ev.working_dir.clone() else { return };
        let Some(engine) = self.engine() else { return };
        if !self.auto_tasks_on() || !safe_auto_folder(&wd) || matches!(self.tasks.running_task_for_dir(&wd), Ok(Some(_))) {
            return;
        }
        if let Err(e) = crate::quick_list::take(&self.db, queue_id, Path::new(&wd), engine.exclude_paths.clone(), received_at, crate::quick_list::ACCEPT_WAIT_MS) {
            eprintln!("quick name list couldn't start: {e}");
        }
    }

    /// Normalizes and stores pending queue entries (no-op while paused).
    pub fn process_pending(&self) -> Result<ProcessStats> {
        let mut st = ProcessStats::default();
        if self.is_paused() {
            return Ok(st);
        }
        for entry in self.queue.drain_pending()? {
            st.processed += 1;
            let ev = match serde_json::from_value::<HookEnvelope>(entry.payload.clone()) {
                Ok(env) => {
                    let _ = self.health.touch(&env.connector);
                    connectors::normalize(&env)
                }
                Err(e) => {
                    let mut ev = AgentEvent::new(&entry.connector, crate::VERSION, EventType::CaptureError, CaptureSource::Hook, None);
                    ev.payload_redacted = json!({"error": format!("malformed hook delivery: {e}"), "received": entry.payload});
                    ev.redact();
                    Some(ev)
                }
            };
            if let Some(mut ev) = ev {
                let phrases = self.custom_redactions.lock().unwrap_or_else(|p| p.into_inner()).clone();
                if !phrases.is_empty() {
                    redact_custom(&mut ev.payload_redacted, &phrases);
                }
                self.attribute(&mut ev)?;
                if ev.task_id.is_none() {
                    self.maybe_auto_start(&ev, &entry.id);
                    self.attribute(&mut ev)?;
                }
                if self.events.insert(&ev)? {
                    st.stored += 1;
                    crate::gitwork::capture::observe(&self.db, &ev);
                    self.maybe_auto_finish(&ev);
                } else {
                    st.duplicates += 1;
                }
            } else {
                st.skipped += 1;
            }
            self.queue.mark_delivered(&entry.id)?;
        }
        Ok(st)
    }

    /// Links an event to the running task whose folders contain the agent's working directory.
    fn attribute(&self, ev: &mut AgentEvent) -> Result<()> {
        if ev.task_id.is_some() {
            return Ok(());
        }
        if let Some(wd) = ev.working_dir.clone() {
            if let Some(tid) = self.tasks.running_task_for_dir(&wd)? {
                if let Some(sid) = &ev.session_id {
                    let t = self.tasks.get(&tid)?;
                    if t.session_id.is_none() {
                        self.tasks.set_session(&tid, sid)?;
                    }
                }
                ev.task_id = Some(tid);
            }
        }
        Ok(())
    }

    /// Background loop: imports the hook's offline file once, then processes the queue.
    pub fn spawn_worker(self: &Arc<Self>, fallback: PathBuf) -> std::thread::JoinHandle<()> {
        let me = self.clone();
        std::thread::spawn(move || {
            if let Err(e) = me.queue.import_fallback_file(&fallback) {
                eprintln!("fallback import failed: {e}");
            }
            let mut ticks = 0u64;
            loop {
                if let Err(e) = me.process_pending() {
                    eprintln!("collector: {e}");
                }
                ticks += 1;
                if ticks % 120 == 0 {
                    let _ = me.queue.import_fallback_file(&fallback);
                    let _ = me.queue.purge_delivered(7 * 24 * 3600 * 1000);
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        })
    }
}

/// Never auto-snapshot a drive root, the home folder itself, or a very short path.
pub fn safe_auto_folder(wd: &str) -> bool {
    let norm = crate::fs_observer::normalize_path(Path::new(wd));
    let segs: Vec<&str> = norm.split(['/', '\\']).filter(|s| !s.is_empty() && !s.ends_with(':')).collect();
    if segs.len() < 2 {
        return false;
    }
    for var in ["USERPROFILE", "HOME"] {
        if let Ok(h) = std::env::var(var) {
            if !h.is_empty() && crate::fs_observer::normalize_path(Path::new(&h)).trim_end_matches(['/', '\\']) == norm.trim_end_matches(['/', '\\']) {
                return false;
            }
        }
    }
    true
}

fn auto_title(ev: &AgentEvent, wd: &str) -> String {
    let prompt = ev.payload_redacted.get("prompt").and_then(Value::as_str).unwrap_or("").trim();
    if !prompt.starts_with("[REDACTED") {
        let t = crate::task::short_title(prompt);
        if !t.is_empty() {
            return t;
        }
    }
    let name = Path::new(wd).file_name().and_then(|n| n.to_str()).unwrap_or(wd);
    format!("Agent job in {name}")
}

fn auto_start(engine: &crate::engine::Engine, ev: &AgentEvent, wd: &str, queue_id: &str) -> Result<()> {
    use crate::task::{FolderRole, NewTask, TaskFolder};
    let instruction = ev.payload_redacted.get("prompt").and_then(Value::as_str).unwrap_or("").to_string();
    let t = engine.tasks.create(NewTask {
        title: auto_title(ev, wd),
        instruction,
        connector: ev.connector_name.clone(),
        folders: vec![TaskFolder { folder_path: wd.to_string(), role: FolderRole::Working }],
        checks: vec![],
    })?;
    engine.tasks.set_origin(&t.id, "auto")?;
    // The quick name list taken when this request arrived belongs to this task.
    let _ = crate::quick_list::claim(&engine.db, queue_id, &t.id);
    engine.capture_baseline(&t.id)?;
    engine.start(&t.id)?;
    Ok(())
}

fn auto_finish(engine: &crate::engine::Engine, t: &crate::task::TaskDefinition) -> Result<()> {
    use crate::task::{FolderRole, NewCheck};
    let mut events = engine.events.by_task(&t.id)?;
    if let Some(sid) = &t.session_id {
        for e in engine.events.by_session(sid)? {
            if !events.iter().any(|x| x.event_id == e.event_id) {
                events.push(e);
            }
        }
    }
    let window = crate::event::task_window(t.created_at, crate::now_ms(), None, &events);
    let events = crate::event::only_this_task(events, window);
    let folder = t.folders_with(FolderRole::Working).into_iter().next().unwrap_or_default();
    let folder_norm = crate::fs_observer::normalize_path(Path::new(&folder));
    let inside: Vec<String> = crate::verifiers::auto_checks::reported_edit_paths(&events)
        .into_iter()
        .filter(|p| crate::fs_observer::relative_to(&folder_norm, p).is_some())
        .collect();
    let mut checks = vec![];
    if !inside.is_empty() {
        checks.push(NewCheck { check_type: "required_outputs_exist".into(), params: json!({"paths": inside, "min_size_bytes": 1}), is_required: true });
    }
    checks.push(NewCheck { check_type: "edits_inside_folder".into(), params: json!({}), is_required: true });
    checks.push(NewCheck { check_type: "no_unexpected_deletions".into(), params: json!({}), is_required: true });
    engine.tasks.update_checks(&t.id, checks)?;
    engine.verify_with_key(&t.id, None)?;
    Ok(())
}
