//! Shared install/uninstall flow for agents that configure hooks in a JSON file.
//! Guarantees: unrelated settings are preserved; the original is backed up once; repeated installs
//! are idempotent; uninstall removes only entries carrying [`OWNER_MARKER`].
use super::{file_sha, hook_command, is_owned_command, load_state, read_json_file, save_state, write_json_file, ConfigPreview, InstallReport, InstallState, UninstallReport};
use crate::{CoreError, Result};
use serde_json::{Map, Value};
use std::path::Path;

pub(crate) enum Upsert {
    Added,
    Updated,
    Unchanged,
}

pub(crate) trait HooksFormat {
    /// Prepare top-level structure (e.g. Cursor's `"version": 1`).
    fn ensure_root(&self, _cfg: &mut Map<String, Value>) {}
    fn upsert(&self, hooks: &mut Map<String, Value>, event: &str, cmd: &str) -> Upsert;
    /// Removes owned hook entries from one event's array; returns how many were removed.
    fn remove_owned(&self, arr: &mut Vec<Value>) -> usize;
    /// All commands configured for an event (any owner).
    fn commands(&self, arr: &[Value]) -> Vec<String>;
}

pub(crate) fn owned_events<F: HooksFormat>(fmt: &F, cfg: &Value) -> Vec<(String, String)> {
    let mut out = vec![];
    if let Some(hooks) = cfg.get("hooks").and_then(Value::as_object) {
        for (ev, arr) in hooks {
            if let Some(arr) = arr.as_array() {
                for c in fmt.commands(arr) {
                    if is_owned_command(&c) {
                        out.push((ev.clone(), c));
                    }
                }
            }
        }
    }
    out
}

/// Pure merge: computes the config AgentTracker would write, without touching disk.
/// Returns (merged config, added events, updated events).
pub(crate) fn merge<F: HooksFormat>(fmt: &F, connector: &str, existing: Option<&Value>, config_path: &Path, events: &[&str], hook_binary: &Path) -> Result<(Value, Vec<String>, Vec<String>)> {
    let mut cfg = existing.cloned().unwrap_or_else(|| Value::Object(Map::new()));
    let Some(root) = cfg.as_object_mut() else {
        return Err(CoreError::Invalid(format!("{} doesn't contain a JSON object; AgentTracker won't modify it", config_path.display())));
    };
    fmt.ensure_root(root);
    let hooks = root.entry("hooks").or_insert_with(|| Value::Object(Map::new()));
    let Some(hooks) = hooks.as_object_mut() else {
        return Err(CoreError::Invalid("\"hooks\" in the config is not an object; AgentTracker won't modify it".into()));
    };
    let (mut added, mut updated) = (vec![], vec![]);
    for ev in events {
        match fmt.upsert(hooks, ev, &hook_command(hook_binary, connector, ev)) {
            Upsert::Added => added.push(ev.to_string()),
            Upsert::Updated => updated.push(ev.to_string()),
            Upsert::Unchanged => {}
        }
    }
    Ok((cfg, added, updated))
}

/// "Show the exact config change before install": before/after text plus a line diff.
pub(crate) fn preview<F: HooksFormat>(fmt: &F, connector: &str, config_path: &Path, events: &[&str], hook_binary: &Path) -> Result<ConfigPreview> {
    let existing = read_json_file(config_path)?;
    let (merged, added, updated) = merge(fmt, connector, existing.as_ref(), config_path, events, hook_binary)?;
    let before = existing.as_ref().map(|v| serde_json::to_string_pretty(v).unwrap_or_default() + "\n").unwrap_or_default();
    let after = serde_json::to_string_pretty(&merged)? + "\n";
    let diff = line_diff(&before, &after);
    Ok(ConfigPreview { config_path: config_path.to_string_lossy().into_owned(), file_exists: existing.is_some(), changes: !added.is_empty() || !updated.is_empty(), added, updated, before, after, diff })
}

/// Minimal LCS line diff: each line prefixed with "  ", "+ " or "- ".
pub fn line_diff(before: &str, after: &str) -> Vec<String> {
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();
    let (n, m) = (a.len(), b.len());
    let mut l = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            l[i][j] = if a[i] == b[j] { l[i + 1][j + 1] + 1 } else { l[i + 1][j].max(l[i][j + 1]) };
        }
    }
    let (mut i, mut j, mut out) = (0, 0, vec![]);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push(format!("  {}", a[i]));
            i += 1;
            j += 1;
        } else if l[i + 1][j] >= l[i][j + 1] {
            out.push(format!("- {}", a[i]));
            i += 1;
        } else {
            out.push(format!("+ {}", b[j]));
            j += 1;
        }
    }
    out.extend(a[i..].iter().map(|s| format!("- {s}")));
    out.extend(b[j..].iter().map(|s| format!("+ {s}")));
    out
}

pub(crate) fn install<F: HooksFormat>(fmt: &F, connector: &str, config_path: &Path, state_file: &Path, events: &[&str], hook_binary: &Path) -> Result<InstallReport> {
    let existing = read_json_file(config_path)?;
    let created_file = existing.is_none();
    let (cfg, added, updated) = merge(fmt, connector, existing.as_ref(), config_path, events, hook_binary)?;
    let prev_state = load_state(state_file);
    let already_ours = existing.as_ref().map(|c| !owned_events(fmt, c).is_empty()).unwrap_or(false);
    let hooks_key_existed = existing.as_ref().map(|c| c.get("hooks").is_some()).unwrap_or(false);

    // Back up the user's original only on the first install (never back up our own output).
    let mut backup_path = prev_state.as_ref().and_then(|s| s.backup_path.clone());
    if !created_file && !already_ours && backup_path.is_none() {
        let b = config_path.with_extension(format!("json.agenttracker-backup-{}", crate::now_ms()));
        std::fs::copy(config_path, &b)?;
        backup_path = Some(b.to_string_lossy().into_owned());
    }
    let written = write_json_file(config_path, &cfg)?;
    let state = InstallState {
        config_path: config_path.to_string_lossy().into_owned(),
        backup_path: backup_path.clone(),
        created_file: prev_state.as_ref().map(|s| s.created_file).unwrap_or(created_file),
        hooks_key_existed: prev_state.as_ref().map(|s| s.hooks_key_existed).unwrap_or(hooks_key_existed),
        written_sha256: written,
        added: events.iter().map(|s| s.to_string()).collect(),
        installed_at: crate::now_ms(),
    };
    save_state(state_file, &state)?;
    Ok(InstallReport { config_path: state.config_path, backup_path, created_file, added, updated })
}

pub(crate) fn uninstall<F: HooksFormat>(fmt: &F, config_path: &Path, state_file: &Path) -> Result<UninstallReport> {
    let state = load_state(state_file);
    let cp = config_path.to_string_lossy().into_owned();
    let Some(mut cfg) = read_json_file(config_path)? else {
        let _ = std::fs::remove_file(state_file);
        return Ok(UninstallReport { config_path: cp, removed: vec![], method: "nothing_to_do".into() });
    };
    let removed_events: Vec<String> = owned_events(fmt, &cfg).into_iter().map(|(e, _)| e).collect();

    // Untouched since our install → put things back exactly as they were.
    if let Some(st) = &state {
        if file_sha(config_path).as_deref() == Some(st.written_sha256.as_str()) {
            if st.created_file {
                std::fs::remove_file(config_path)?;
                let _ = std::fs::remove_file(state_file);
                return Ok(UninstallReport { config_path: cp, removed: removed_events, method: "deleted_created_file".into() });
            }
            if let Some(b) = st.backup_path.as_ref().filter(|b| Path::new(b).exists()) {
                std::fs::copy(b, config_path)?;
                let _ = std::fs::remove_file(b);
                let _ = std::fs::remove_file(state_file);
                return Ok(UninstallReport { config_path: cp, removed: removed_events, method: "restored_backup".into() });
            }
        }
    }

    // The user edited the file since: remove only our entries.
    if removed_events.is_empty() {
        let _ = std::fs::remove_file(state_file);
        return Ok(UninstallReport { config_path: cp, removed: vec![], method: "nothing_to_do".into() });
    }
    if let Some(root) = cfg.as_object_mut() {
        let mut drop_hooks = false;
        if let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) {
            let keys: Vec<String> = hooks.keys().cloned().collect();
            for k in keys {
                if let Some(arr) = hooks.get_mut(&k).and_then(Value::as_array_mut) {
                    if fmt.remove_owned(arr) > 0 && arr.is_empty() {
                        hooks.remove(&k);
                    }
                }
            }
            drop_hooks = hooks.is_empty() && !state.as_ref().map(|s| s.hooks_key_existed).unwrap_or(true);
        }
        if drop_hooks {
            root.remove("hooks");
        }
    }
    write_json_file(config_path, &cfg)?;
    let _ = std::fs::remove_file(state_file);
    Ok(UninstallReport { config_path: cp, removed: removed_events, method: "surgical_removal".into() })
}
