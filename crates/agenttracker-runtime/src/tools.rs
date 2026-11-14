//! The runtime's confined tool set and its single dispatcher.
//!
//! Enforcement boundary: these checks constrain only the tools this runtime exposes to the model.
//! They do not sandbox the operating system, other processes or other agents on the computer.
//! There is no shell and no network tool.
use serde_json::{json, Value};
use std::path::{Component, Path, PathBuf};

pub const MAX_READ_BYTES: usize = 32 * 1024;
pub const MAX_WRITE_BYTES: usize = 256 * 1024;
pub const MAX_LIST_ENTRIES: usize = 300;

pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    /// Argument names (all strings) and whether they are paths.
    pub args: &'static [(&'static str, bool)],
    /// Paths this tool may modify (argument names).
    pub writes: &'static [&'static str],
    pub idempotent: bool,
}

pub const TOOLS: &[ToolDef] = &[
    ToolDef { name: "list_directory", description: "List files and folders under a relative path in the working folder.", args: &[("path", true)], writes: &[], idempotent: true },
    ToolDef { name: "read_text_file", description: "Read a UTF-8 text file (first 32 KB) from the working folder.", args: &[("path", true)], writes: &[], idempotent: true },
    ToolDef { name: "copy_file", description: "Copy a file to a new path. Fails if the destination exists. The source is left unchanged.", args: &[("source", true), ("destination", true)], writes: &["destination"], idempotent: false },
    ToolDef { name: "write_text_file", description: "Write UTF-8 text to a file, creating or replacing it.", args: &[("path", true), ("content", false)], writes: &["path"], idempotent: false },
];

pub fn tool(name: &str) -> Option<&'static ToolDef> {
    TOOLS.iter().find(|t| t.name == name)
}

/// Tool definitions as given to the model (and recorded in every snapshot).
pub fn definitions_json() -> Value {
    Value::Array(
        TOOLS
            .iter()
            .map(|t| {
                let props: serde_json::Map<String, Value> = t.args.iter().map(|(a, p)| (a.to_string(), json!({"type": "string", "description": if *p { "path relative to the working folder" } else { "text" }}))).collect();
                json!({"name": t.name, "description": t.description, "arguments": {"type": "object", "properties": props, "required": t.args.iter().map(|(a, _)| *a).collect::<Vec<_>>()}})
            })
            .collect(),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    Empty,
    Absolute,
    Traversal,
    Escapes,
    Protected(String),
}

impl PathError {
    pub fn message(&self, arg: &str) -> String {
        match self {
            PathError::Empty => format!("'{arg}' is empty"),
            PathError::Absolute => format!("'{arg}' must be relative to the working folder"),
            PathError::Traversal => format!("'{arg}' may not contain '..' or drive/root components"),
            PathError::Escapes => format!("'{arg}' resolves outside the working folder"),
            PathError::Protected(p) => format!("'{arg}' targets protected path '{p}'"),
        }
    }
}

/// Normalises a model-supplied relative path to forward slashes without `.` components.
pub fn normalize_rel(rel: &str) -> Result<String, PathError> {
    let r = rel.trim().replace('\\', "/");
    if r.is_empty() {
        return Err(PathError::Empty);
    }
    if r.starts_with('/') || r.contains(':') || r.contains('\0') {
        return Err(PathError::Absolute);
    }
    let mut parts = Vec::new();
    for c in Path::new(&r).components() {
        match c {
            Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
            Component::CurDir => {}
            _ => return Err(PathError::Traversal),
        }
    }
    if parts.is_empty() {
        return Ok(".".into());
    }
    Ok(parts.join("/"))
}

pub fn is_protected(rel_norm: &str, protected: &[String]) -> Option<String> {
    let lower = rel_norm.to_lowercase();
    protected.iter().find_map(|p| {
        let pn = normalize_rel(p).ok()?.to_lowercase();
        let hit = pn == "." || lower == pn || lower.starts_with(&format!("{pn}/"));
        hit.then(|| p.clone())
    })
}

/// Resolves a relative path inside `root`, rejecting symlink escapes by canonicalising the deepest
/// existing ancestor.
pub fn resolve(root: &Path, rel: &str) -> Result<PathBuf, PathError> {
    let norm = normalize_rel(rel)?;
    let root_c = root.canonicalize().map_err(|_| PathError::Escapes)?;
    let target = if norm == "." { root_c.clone() } else { root_c.join(&norm) };
    let mut probe = target.clone();
    loop {
        if probe.exists() {
            let c = probe.canonicalize().map_err(|_| PathError::Escapes)?;
            if !c.starts_with(&root_c) {
                return Err(PathError::Escapes);
            }
            break;
        }
        if !probe.pop() {
            return Err(PathError::Escapes);
        }
    }
    Ok(target)
}

/// Static policy check used during validation (before any execution).
pub fn check_args(tool: &ToolDef, args: &Value, root: &Path, protected: &[String]) -> Vec<(String, String)> {
    let mut errs = Vec::new();
    let Some(obj) = args.as_object() else {
        return vec![("bad_arguments".into(), "arguments must be an object".into())];
    };
    for k in obj.keys() {
        if !tool.args.iter().any(|(a, _)| a == k) {
            errs.push(("bad_arguments".into(), format!("unexpected argument '{k}' for {}", tool.name)));
        }
    }
    for (a, is_path) in tool.args {
        match obj.get(*a).and_then(|v| v.as_str()) {
            None => errs.push(("bad_arguments".into(), format!("missing string argument '{a}'"))),
            Some(v) if *is_path => {
                if v.len() > 512 {
                    errs.push(("field_too_long".into(), format!("'{a}' exceeds 512 characters")));
                    continue;
                }
                if let Err(e) = resolve(root, v) {
                    errs.push(("path_denied".into(), e.message(a)));
                    continue;
                }
                if tool.writes.contains(a) {
                    if let Some(p) = normalize_rel(v).ok().and_then(|n| is_protected(&n, protected)) {
                        errs.push(("protected_path".into(), PathError::Protected(p).message(a)));
                    }
                }
            }
            Some(v) => {
                if v.len() > MAX_WRITE_BYTES {
                    errs.push(("field_too_long".into(), format!("'{a}' exceeds {MAX_WRITE_BYTES} bytes")));
                }
            }
        }
    }
    errs
}

#[derive(Debug, Clone)]
pub struct ToolOutcome {
    pub ok: bool,
    pub output: Value,
    pub error: Option<String>,
}

fn fail(msg: impl Into<String>) -> ToolOutcome {
    ToolOutcome { ok: false, output: Value::Null, error: Some(msg.into()) }
}

/// The single dispatcher. Policy is re-checked here (defence in depth) even though validation has
/// already run, so a stored decision cannot bypass path or protection rules.
pub fn dispatch(name: &str, args: &Value, root: &Path, protected: &[String]) -> ToolOutcome {
    let Some(def) = tool(name) else { return fail(format!("unknown tool {name}")) };
    let errs = check_args(def, args, root, protected);
    if !errs.is_empty() {
        return fail(errs.into_iter().map(|(_, m)| m).collect::<Vec<_>>().join("; "));
    }
    let s = |k: &str| args[k].as_str().unwrap_or_default().to_string();
    let path = |k: &str| resolve(root, &s(k)).expect("checked");
    match name {
        "list_directory" => {
            let dir = path("path");
            let rd = match std::fs::read_dir(&dir) {
                Ok(r) => r,
                Err(e) => return fail(format!("cannot list: {e}")),
            };
            let mut entries: Vec<Value> = rd
                .filter_map(|e| e.ok())
                .map(|e| {
                    let ft = e.file_type().ok();
                    json!({"name": e.file_name().to_string_lossy(), "type": if ft.map(|f| f.is_dir()).unwrap_or(false) { "dir" } else { "file" }, "bytes": e.metadata().ok().map(|m| m.len())})
                })
                .collect();
            entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
            let truncated = entries.len() > MAX_LIST_ENTRIES;
            entries.truncate(MAX_LIST_ENTRIES);
            ToolOutcome { ok: true, output: json!({"path": s("path"), "entries": entries, "truncated": truncated}), error: None }
        }
        "read_text_file" => match std::fs::read(path("path")) {
            Ok(bytes) => {
                let truncated = bytes.len() > MAX_READ_BYTES;
                let slice = &bytes[..bytes.len().min(MAX_READ_BYTES)];
                match std::str::from_utf8(slice) {
                    Ok(t) => ToolOutcome { ok: true, output: json!({"path": s("path"), "text": t, "bytes": bytes.len(), "truncated": truncated}), error: None },
                    Err(_) if truncated => ToolOutcome { ok: true, output: json!({"path": s("path"), "text": String::from_utf8_lossy(slice), "bytes": bytes.len(), "truncated": true}), error: None },
                    Err(_) => fail("file is not UTF-8 text"),
                }
            }
            Err(e) => fail(format!("cannot read: {e}")),
        },
        "copy_file" => {
            let (src, dst) = (path("source"), path("destination"));
            if !src.is_file() {
                return fail("source is not a file");
            }
            if dst.exists() {
                return fail("destination already exists (copy_file never overwrites)");
            }
            if let Some(parent) = dst.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    return fail(format!("cannot create destination folder: {e}"));
                }
            }
            // Re-check after creating parents (a symlinked folder could have appeared).
            if resolve(root, &s("destination")).is_err() {
                return fail("destination escapes the working folder");
            }
            match std::fs::copy(&src, &dst) {
                Ok(n) => ToolOutcome { ok: true, output: json!({"source": s("source"), "destination": s("destination"), "bytes_copied": n}), error: None },
                Err(e) => fail(format!("copy failed: {e}")),
            }
        }
        "write_text_file" => {
            let dst = path("path");
            if dst.is_dir() {
                return fail("path is a folder");
            }
            if let Some(parent) = dst.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    return fail(format!("cannot create folder: {e}"));
                }
            }
            let existed = dst.exists();
            match std::fs::write(&dst, s("content")) {
                Ok(()) => ToolOutcome { ok: true, output: json!({"path": s("path"), "bytes_written": s("content").len(), "replaced_existing": existed}), error: None },
                Err(e) => fail(format!("write failed: {e}")),
            }
        }
        _ => fail("unsupported tool"),
    }
}
