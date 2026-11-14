//! Where the engine keeps its own data and which local endpoint it listens on.
//! Deliberately separate from Houndbit's data folder (com.agenttracker.app).
use std::path::PathBuf;

pub fn default_data_dir() -> PathBuf {
    if cfg!(windows) {
        if let Ok(l) = std::env::var("LOCALAPPDATA") {
            return PathBuf::from(l).join("HoundbitProtect");
        }
    }
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".local").join("share").join("houndbit-protect")
}

fn user_tag() -> String {
    let u = std::env::var("USERNAME").or_else(|_| std::env::var("USER")).unwrap_or_else(|_| "user".into());
    u.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' }).collect()
}

/// One engine per Windows user: the pipe name includes the user name.
pub fn default_endpoint(data_dir: &std::path::Path) -> String {
    if cfg!(windows) {
        format!(r"\\.\pipe\houndbit-protect-v1-{}", user_tag())
    } else {
        data_dir.join("engine.sock").to_string_lossy().into_owned()
    }
}
