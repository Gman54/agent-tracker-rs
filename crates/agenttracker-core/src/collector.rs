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
        })
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
        let v: Value = serde_json::from_slice(body).unwrap_or_else(|e| json!({"connector": "unknown", "malformed": e.to_string()}));
        let connector = v.get("connector").and_then(Value::as_str).unwrap_or("unknown").to_string();
        self.queue.enqueue(&connector, &v)
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
                if self.events.insert(&ev)? {
                    st.stored += 1;
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
