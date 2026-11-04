//! Built-in harmless demo task (copied from the repo's demo/ folder at compile time).
use crate::task::NewTask;
use crate::Result;
use std::path::{Path, PathBuf};

const TASK_JSON: &str = include_str!("../../../demo/task.json");
const FILES: &[(&str, &str)] = &[
    ("input/shopping_list.txt", include_str!("../../../demo/input/shopping_list.txt")),
    ("input/meeting_notes.txt", include_str!("../../../demo/input/meeting_notes.txt")),
    ("input/ideas.txt", include_str!("../../../demo/input/ideas.txt")),
    ("expected_output/.gitkeep", ""),
];

/// Resolves relative folder paths in a task definition against `base`.
pub fn resolve_task_paths(mut t: NewTask, base: &Path) -> NewTask {
    for f in t.folders.iter_mut() {
        let p = Path::new(&f.folder_path);
        if !p.is_absolute() {
            let joined = if f.folder_path == "." { base.to_path_buf() } else { base.join(p) };
            f.folder_path = joined.to_string_lossy().into_owned();
        }
    }
    t
}

/// Writes fresh demo files into `dir` (a new timestamped subfolder) and returns the task to create.
pub fn install_demo(dir: &Path) -> Result<(NewTask, PathBuf)> {
    let base = dir.join(format!("demo-{}", crate::now_ms()));
    for (rel, content) in FILES {
        let p = base.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(p, content)?;
    }
    let task: NewTask = serde_json::from_str(TASK_JSON)?;
    Ok((resolve_task_paths(task, &base), base))
}
