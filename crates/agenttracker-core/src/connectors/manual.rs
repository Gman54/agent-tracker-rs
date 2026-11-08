//! Manual connector (spec §9): no agent integration. The user picks folders and checks,
//! TrailFox captures a "before" snapshot, the user (or any tool) does the work, then
//! "Verify now" captures the "after" snapshot and runs the checks.
use super::{Capability, ConnectorInfo, DetectResult, Maturity};
use crate::engine::Engine;
use crate::event::EventType;
use crate::store::VerificationRun;
use crate::task::{NewTask, TaskDefinition};
use crate::Result;

pub const NAME: &str = "manual";
pub const LABEL: &str = "File verification only. Agent actions and attribution are unavailable.";

pub fn info() -> ConnectorInfo {
    ConnectorInfo { name: NAME, display_name: "Manual folder check", maturity: Maturity::FileOnly, status_note: LABEL, docs_checked: "n/a" }
}

pub struct ManualConnector;

impl ManualConnector {
    pub fn detect() -> DetectResult {
        DetectResult {
            connector: NAME.into(),
            agent_found: true,
            agent_version: None,
            config_path: String::new(),
            config_exists: true,
            hooks_installed: false,
            notes: vec![LABEL.into()],
        }
    }

    pub fn capabilities() -> Vec<Capability> {
        [
            EventType::SessionStarted,
            EventType::TaskSubmitted,
            EventType::ToolStarted,
            EventType::ToolFinished,
            EventType::AgentReportedCompletion,
            EventType::SessionEnded,
        ]
        .into_iter()
        .map(|t| Capability { event_type: t, available: false, detail: LABEL.into() })
        .collect()
    }

    /// Step 1+2: create the task and capture the "before" snapshot.
    pub fn begin(engine: &Engine, mut task: NewTask) -> Result<TaskDefinition> {
        task.connector = NAME.into();
        let t = engine.tasks.create(task)?;
        engine.capture_baseline(&t.id)
    }

    /// Step 4: "Verify now".
    pub fn verify_now(engine: &Engine, task_id: &str) -> Result<VerificationRun> {
        engine.verify(task_id)
    }
}
