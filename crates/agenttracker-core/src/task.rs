//! Task definitions (spec §6) and the lifecycle state machine.
//! Editing checks/folders creates a new definition version; a verification run freezes (snapshots)
//! the version it used so results always refer to an immutable definition.
use crate::db::Db;
use crate::{CoreError, Result};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    Draft,
    Baselining,
    Ready,
    Running,
    Verifying,
    Completed,
    Cancelled,
    Error,
}

impl Lifecycle {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Baselining => "baselining",
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Verifying => "verifying",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Error => "error",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        serde_json::from_value(Value::String(s.into())).ok()
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Cancelled)
    }

    /// Allowed transitions. Main path: draft→baselining→ready→running→verifying→completed.
    /// Also: ready→verifying (verify without a tracked run), completed→verifying (re-verify),
    /// error→baselining (retry), any non-terminal→cancelled|error, baselining→draft (abort capture).
    pub fn can_transition(self, to: Lifecycle) -> bool {
        use Lifecycle::*;
        if self.is_terminal() {
            return false;
        }
        if matches!(to, Cancelled | Error) {
            return self != to;
        }
        matches!(
            (self, to),
            (Draft, Baselining)
                | (Baselining, Ready)
                | (Baselining, Draft)
                | (Ready, Running)
                | (Ready, Verifying)
                | (Ready, Baselining)
                | (Running, Verifying)
                | (Verifying, Completed)
                | (Completed, Verifying)
                | (Error, Baselining)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FolderRole {
    Working,
    Protected,
    AllowedWrite,
}

impl FolderRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Protected => "protected",
            Self::AllowedWrite => "allowed_write",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        serde_json::from_value(Value::String(s.into())).ok()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskFolder {
    pub folder_path: String,
    pub role: FolderRole,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskCheck {
    pub id: String,
    pub check_type: String,
    #[serde(default)]
    pub params: Value,
    #[serde(default = "yes")]
    pub is_required: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskDefinition {
    pub id: String,
    pub title: String,
    pub instruction: String,
    /// Connector the user expects to do the work ("claude_code", "cursor", "manual", ...).
    pub connector: String,
    pub definition_version: i64,
    pub lifecycle: Lifecycle,
    pub folders: Vec<TaskFolder>,
    pub checks: Vec<TaskCheck>,
    /// Agent session linked to this task (set when events are attributed).
    pub session_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl TaskDefinition {
    pub fn folders_with(&self, role: FolderRole) -> Vec<String> {
        self.folders.iter().filter(|f| f.role == role).map(|f| f.folder_path.clone()).collect()
    }
    /// Distinct roots to snapshot: working + protected folders (allowed_write lie inside them).
    pub fn snapshot_roots(&self) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        for f in &self.folders {
            if matches!(f.role, FolderRole::Working | FolderRole::Protected) && !v.contains(&f.folder_path) {
                v.push(f.folder_path.clone());
            }
        }
        v
    }
}

/// Input for creating a task (from the UI or demo/task.json).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NewTask {
    pub title: String,
    #[serde(default)]
    pub instruction: String,
    #[serde(default = "manual")]
    pub connector: String,
    #[serde(default)]
    pub folders: Vec<TaskFolder>,
    #[serde(default)]
    pub checks: Vec<NewCheck>,
}

fn manual() -> String {
    "manual".into()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NewCheck {
    pub check_type: String,
    #[serde(default)]
    pub params: Value,
    #[serde(default = "yes")]
    pub is_required: bool,
}

#[derive(Clone)]
pub struct TaskStore {
    db: Db,
}

impl TaskStore {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    pub fn create(&self, t: NewTask) -> Result<TaskDefinition> {
        if t.title.trim().is_empty() {
            return Err(CoreError::Invalid("A task needs a title".into()));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let now = crate::now_ms();
        let conn = self.db.conn();
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO tasks(id, title, instruction, definition_version, lifecycle, created_at, updated_at, connector) VALUES (?1,?2,?3,1,'draft',?4,?4,?5)",
            params![id, t.title.trim(), t.instruction, now, t.connector],
        )?;
        for f in &t.folders {
            tx.execute("INSERT INTO task_folders(task_id, folder_path, role) VALUES (?1,?2,?3)", params![id, f.folder_path, f.role.as_str()])?;
        }
        for c in &t.checks {
            tx.execute(
                "INSERT INTO task_checks(id, task_id, check_type, params, is_required, definition_version) VALUES (?1,?2,?3,?4,?5,1)",
                params![uuid::Uuid::new_v4().to_string(), id, c.check_type, c.params.to_string(), c.is_required as i64],
            )?;
        }
        tx.commit()?;
        drop(conn);
        self.get(&id)
    }

    pub fn get(&self, id: &str) -> Result<TaskDefinition> {
        let conn = self.db.conn();
        let row = conn
            .query_row(
                "SELECT id, title, instruction, definition_version, lifecycle, created_at, updated_at, connector, session_id FROM tasks WHERE id = ?1",
                [id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, i64>(5)?,
                        r.get::<_, i64>(6)?,
                        r.get::<_, String>(7)?,
                        r.get::<_, Option<String>>(8)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| CoreError::NotFound(format!("task {id}")))?;
        let mut fs = conn.prepare("SELECT folder_path, role FROM task_folders WHERE task_id = ?1 ORDER BY rowid")?;
        let folders = fs
            .query_map([id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .filter_map(|r| r.ok())
            .filter_map(|(p, role)| FolderRole::parse(&role).map(|role| TaskFolder { folder_path: p, role }))
            .collect();
        let mut cs = conn.prepare("SELECT id, check_type, params, is_required FROM task_checks WHERE task_id = ?1 AND definition_version = ?2 ORDER BY rowid")?;
        let checks = cs
            .query_map(params![id, row.3], |r| {
                Ok(TaskCheck {
                    id: r.get(0)?,
                    check_type: r.get(1)?,
                    params: serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or(Value::Null),
                    is_required: r.get::<_, i64>(3)? != 0,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(TaskDefinition {
            id: row.0,
            title: row.1,
            instruction: row.2.unwrap_or_default(),
            definition_version: row.3,
            lifecycle: Lifecycle::parse(&row.4).unwrap_or(Lifecycle::Error),
            folders,
            checks,
            session_id: row.8,
            created_at: row.5,
            updated_at: row.6,
            connector: row.7,
        })
    }

    pub fn list(&self) -> Result<Vec<TaskDefinition>> {
        let ids: Vec<String> = {
            let conn = self.db.conn();
            let mut s = conn.prepare("SELECT id FROM tasks ORDER BY updated_at DESC")?;
            let v = s.query_map([], |r| r.get(0))?.collect::<std::result::Result<Vec<String>, _>>()?;
            v
        };
        ids.iter().map(|id| self.get(id)).collect()
    }

    /// Enforced lifecycle transition.
    pub fn transition(&self, id: &str, to: Lifecycle) -> Result<TaskDefinition> {
        let t = self.get(id)?;
        if !t.lifecycle.can_transition(to) {
            return Err(CoreError::Lifecycle { from: t.lifecycle.as_str().into(), to: to.as_str().into() });
        }
        self.db.conn().execute("UPDATE tasks SET lifecycle = ?2, updated_at = ?3 WHERE id = ?1", params![id, to.as_str(), crate::now_ms()])?;
        self.get(id)
    }

    /// Replaces the check list. Always creates a new definition version (old versions remain
    /// queryable for past verification runs).
    pub fn update_checks(&self, id: &str, checks: Vec<NewCheck>) -> Result<TaskDefinition> {
        let t = self.get(id)?;
        if matches!(t.lifecycle, Lifecycle::Verifying | Lifecycle::Cancelled) {
            return Err(CoreError::Invalid(format!("checks can't be edited while the task is {}", t.lifecycle.as_str())));
        }
        let v = t.definition_version + 1;
        let conn = self.db.conn();
        let tx = conn.unchecked_transaction()?;
        for c in &checks {
            tx.execute(
                "INSERT INTO task_checks(id, task_id, check_type, params, is_required, definition_version) VALUES (?1,?2,?3,?4,?5,?6)",
                params![uuid::Uuid::new_v4().to_string(), id, c.check_type, c.params.to_string(), c.is_required as i64, v],
            )?;
        }
        tx.execute("UPDATE tasks SET definition_version = ?2, updated_at = ?3 WHERE id = ?1", params![id, v, crate::now_ms()])?;
        tx.commit()?;
        drop(conn);
        self.get(id)
    }

    pub fn set_session(&self, id: &str, session_id: &str) -> Result<()> {
        self.db.conn().execute("UPDATE tasks SET session_id = ?2, updated_at = ?3 WHERE id = ?1", params![id, session_id, crate::now_ms()])?;
        Ok(())
    }

    /// Freezes the current definition version (idempotent). Returns the frozen version number.
    pub fn freeze(&self, id: &str) -> Result<i64> {
        let t = self.get(id)?;
        self.db.conn().execute(
            "INSERT OR IGNORE INTO task_definition_snapshots(task_id, definition_version, snapshot, frozen_at) VALUES (?1,?2,?3,?4)",
            params![id, t.definition_version, serde_json::to_string(&t)?, crate::now_ms()],
        )?;
        Ok(t.definition_version)
    }

    pub fn snapshot(&self, id: &str, version: i64) -> Result<Option<TaskDefinition>> {
        let s: Option<String> = self
            .db
            .conn()
            .query_row("SELECT snapshot FROM task_definition_snapshots WHERE task_id = ?1 AND definition_version = ?2", params![id, version], |r| r.get(0))
            .optional()?;
        Ok(s.and_then(|s| serde_json::from_str(&s).ok()))
    }

    /// Task currently in the `running` state for a working dir (used to attribute agent events).
    pub fn running_task_for_dir(&self, working_dir: &str) -> Result<Option<String>> {
        let norm = crate::fs_observer::normalize_path(std::path::Path::new(working_dir));
        for t in self.list()? {
            if t.lifecycle != Lifecycle::Running {
                continue;
            }
            for f in &t.folders {
                let root = crate::fs_observer::normalize_path(std::path::Path::new(&f.folder_path));
                if crate::fs_observer::relative_to(&root, &norm).is_some() || crate::fs_observer::relative_to(&norm, &root).is_some() {
                    return Ok(Some(t.id.clone()));
                }
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::Lifecycle::*;

    #[test]
    fn transitions() {
        assert!(Draft.can_transition(Baselining));
        assert!(!Draft.can_transition(Running));
        assert!(Running.can_transition(Verifying));
        assert!(!Cancelled.can_transition(Draft));
        assert!(Ready.can_transition(Cancelled));
        assert!(!Completed.can_transition(Running));
    }
}
