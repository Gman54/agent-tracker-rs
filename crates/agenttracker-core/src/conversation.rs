//! Claude Code's own session record ("transcript"), shown next to the checks on a task report.
//!
//! Claude Code passes `transcript_path` to every hook. It points at a JSONL file under
//! `~/.claude/projects/...`. The format is not documented and changes between versions, so this
//! reader is deliberately forgiving: unknown or broken lines are skipped and counted, never fatal.
//! We only ever read the file; it is never changed.
use crate::event::AgentEvent;
use crate::store::VerificationRun;
use crate::task::TaskDefinition;
use serde::Serialize;
use serde_json::{json, Value};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use tracker_common::redaction::redact_text;

/// Only the newest part of a big record is read.
pub const MAX_READ_BYTES: u64 = 5 * 1024 * 1024;
/// Most entries kept (the newest ones).
pub const MAX_ENTRIES: usize = 3000;
/// Longest text kept per entry; the page shows a short preview with "Show more".
pub const MAX_TEXT_CHARS: usize = 20_000;
/// Short summaries for action inputs.
const MAX_INPUT_CHARS: usize = 300;
/// Leeway around the task's time window (clocks and write order are not exact).

/// Folders Claude Code keeps its records in: `~/.claude`, plus `CLAUDE_CONFIG_DIR` when set.
pub fn claude_roots() -> Vec<PathBuf> {
    let mut roots = vec![crate::user_home().join(".claude")];
    if let Some(d) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty()) {
        roots.push(PathBuf::from(d));
    }
    roots
}

fn is_jsonl(p: &Path) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("jsonl"))
}

fn no_parent_steps(p: &Path) -> bool {
    !p.components().any(|c| matches!(c, Component::ParentDir))
}

/// Capture-time check on a hook's `transcript_path`: an absolute `.jsonl` path inside a `.claude`
/// folder (or a configured Claude folder), with no `..` steps. Anything else is not stored.
/// The strict check against the real folder happens again when the file is read.
pub fn plausible_transcript_path(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > 1024 {
        return None;
    }
    let p = Path::new(raw);
    if !p.is_absolute() || !is_jsonl(p) || !no_parent_steps(p) {
        return None;
    }
    let in_dot_claude = p.components().any(|c| matches!(c, Component::Normal(s) if s.eq_ignore_ascii_case(".claude")));
    let in_root = claude_roots().iter().any(|r| lexically_inside(p, r));
    (in_dot_claude || in_root).then(|| raw.to_string())
}

fn lexically_inside(p: &Path, root: &Path) -> bool {
    if cfg!(windows) {
        let (p, r) = (p.to_string_lossy().to_lowercase(), root.to_string_lossy().to_lowercase());
        Path::new(&p).starts_with(Path::new(&r))
    } else {
        p.starts_with(root)
    }
}

/// Read-time check: the path must be a `.jsonl` file that, after resolving links, really lives
/// inside one of `roots`. Returns the resolved path.
pub fn safe_transcript_path(raw: &str, roots: &[PathBuf]) -> Option<PathBuf> {
    let p = Path::new(raw.trim());
    if !p.is_absolute() || !is_jsonl(p) || !no_parent_steps(p) {
        return None;
    }
    let real = std::fs::canonicalize(p).ok()?;
    if !real.is_file() || !is_jsonl(&real) {
        return None;
    }
    roots.iter().filter_map(|r| std::fs::canonicalize(r).ok()).any(|r| lexically_inside(&real, &r)).then_some(real)
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    /// Your message.
    You,
    /// Claude's reply.
    Claude,
    /// The thinking Claude chose to show (may be a summary).
    Thinking,
    /// A tool Claude used.
    Action,
    /// What came back from that tool.
    Result,
}

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub kind: EntryKind,
    /// Milliseconds since 1970, when the record has a usable time.
    pub ts: Option<i64>,
    pub text: String,
    /// True when `text` was cut to `MAX_TEXT_CHARS`.
    pub cut: bool,
    /// Tool name for actions and results (when known).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    pub is_error: bool,
    /// Claude Code's id for the tool call (on actions and their results). Hooks send the same id,
    /// so an action here can be matched to the start/finish records Houndbit captured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    /// Actions only: your latest request before this action (shortened).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
    /// Actions only: what Claude wrote just before this action, since your last message or the
    /// last tool result. Its own written explanation, not verified reasoning.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note_before: Option<String>,
}

const MAX_INSTRUCTION_CHARS: usize = 200;
const MAX_NOTE_CHARS: usize = 600;

fn shorten(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(max) {
        Some((i, _)) => format!("{}\u{2026}", flat[..i].trim_end()),
        None => flat,
    }
}

/// Links each action to your latest request and to what Claude wrote right before it. Order-based:
/// the note is the Claude text since your last message or the last tool result, so actions Claude
/// starts together share the same note. Thinking is not included (it's shown separately).
fn link_notes(entries: &mut [Entry]) {
    let mut instruction: Option<String> = None;
    let mut note: Vec<String> = vec![];
    for e in entries.iter_mut() {
        match e.kind {
            EntryKind::You => {
                instruction = Some(shorten(&e.text, MAX_INSTRUCTION_CHARS));
                note.clear();
            }
            EntryKind::Result => note.clear(),
            EntryKind::Claude => {
                if !crate::report::looks_like_api_error(&e.text) {
                    note.push(e.text.clone());
                }
            }
            EntryKind::Action => {
                e.instruction = instruction.clone();
                e.note_before = if note.is_empty() { None } else { Some(shorten(&note.join(" "), MAX_NOTE_CHARS)) };
            }
            EntryKind::Thinking => {}
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Parsed {
    pub entries: Vec<Entry>,
    /// Lines that were broken or of a kind we don't show.
    pub skipped_lines: usize,
    pub total_lines: usize,
    /// True when entries were limited to the task's time window.
    pub windowed: bool,
    /// True when older entries were dropped to stay under `MAX_ENTRIES`.
    pub capped: bool,
}

fn cut_text(s: &str, max: usize) -> (String, bool) {
    let s = redact_text(s.trim());
    match s.char_indices().nth(max) {
        Some((i, _)) => (s[..i].to_string(), true),
        None => (s, false),
    }
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Parses RFC 3339 times like `2026-11-05T14:03:22.123Z` or `...+02:00` into ms. None if unusual.
pub fn parse_ts(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !(b[10] == b'T' || b[10] == b' ') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (num(0..4)?, num(5..7)?, num(8..10)?, num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let mut i = 19;
    let mut ms = 0i64;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        let frac = &s[start..i];
        let first3: String = frac.chars().chain("000".chars()).take(3).collect();
        ms = first3.parse().ok()?;
    }
    let offset_min = match b.get(i) {
        None | Some(b'Z') | Some(b'z') => 0,
        Some(b'+') | Some(b'-') => {
            let sign = if b[i] == b'-' { -1 } else { 1 };
            let oh = num(i + 1..i + 3)?;
            let om = if b.get(i + 3) == Some(&b':') { num(i + 4..i + 6)? } else { num(i + 3..i + 5).unwrap_or(0) };
            sign * (oh * 60 + om)
        }
        _ => return None,
    };
    let secs = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se - offset_min * 60;
    Some(secs * 1000 + ms)
}

/// One-line description of what a tool was asked to do.
fn input_summary(tool: &str, input: &Value) -> String {
    let field = |k: &str| input.get(k).and_then(Value::as_str).map(str::to_string);
    let picked = match tool {
        "Bash" | "PowerShell" => field("command"),
        "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => field("file_path").or_else(|| field("notebook_path")),
        "Glob" | "Grep" => field("pattern").map(|p| match field("path") {
            Some(dir) => format!("{p} in {dir}"),
            None => p,
        }),
        "WebFetch" => field("url"),
        "WebSearch" => field("query"),
        "Task" | "Agent" => field("description").or_else(|| field("prompt")),
        "TodoWrite" => Some("updated its to-do list".into()),
        _ => None,
    };
    let s = picked.unwrap_or_else(|| match input {
        Value::Null => String::new(),
        Value::Object(m) if m.is_empty() => String::new(),
        other => other.to_string(),
    });
    cut_text(&s.replace(['\r', '\n'], " "), MAX_INPUT_CHARS).0
}

/// Text inside a tool_result block: a string, or a list of `{type:"text", text}` (images skipped).
fn result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|b| match b.get("type").and_then(Value::as_str) {
                Some("text") => b.get("text").and_then(Value::as_str).map(str::to_string),
                Some("image") => Some("[an image]".into()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) if !other.is_null() => other.to_string(),
        _ => String::new(),
    }
}

fn push<'a>(out: &'a mut Vec<Entry>, kind: EntryKind, ts: Option<i64>, text: &str, tool: Option<String>, is_error: bool) -> Option<&'a mut Entry> {
    let (text, cut) = cut_text(text, MAX_TEXT_CHARS);
    if text.is_empty() && !matches!(kind, EntryKind::Action | EntryKind::Result) {
        return None;
    }
    out.push(Entry { kind, ts, text, cut, tool, is_error, tool_use_id: None, instruction: None, note_before: None });
    out.last_mut()
}

/// Turns one JSONL line into entries. Returns None when the line isn't something we show.
fn parse_line(v: &Value, tool_names: &mut std::collections::HashMap<String, String>) -> Option<Vec<Entry>> {
    let kind = v.get("type").and_then(Value::as_str)?;
    if !matches!(kind, "user" | "assistant") {
        return None;
    }
    // Housekeeping lines (slash-command echoes, caveats) and sub-agent side chats are left out.
    if v.get("isMeta").and_then(Value::as_bool) == Some(true) || v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let ts = v.get("timestamp").and_then(Value::as_str).and_then(parse_ts);
    let msg = v.get("message")?;
    let content = msg.get("content")?;
    let mut out = vec![];
    let speaker = if kind == "user" { EntryKind::You } else { EntryKind::Claude };
    match content {
        Value::String(s) => {
            push(&mut out, speaker, ts, s, None, false);
        }
        Value::Array(blocks) => {
            for b in blocks {
                match b.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        push(&mut out, speaker.clone(), ts, b.get("text").and_then(Value::as_str).unwrap_or(""), None, false);
                    }
                    Some("thinking") => {
                        push(&mut out, EntryKind::Thinking, ts, b.get("thinking").and_then(Value::as_str).unwrap_or(""), None, false);
                    }
                    Some("tool_use") => {
                        let name = b.get("name").and_then(Value::as_str).unwrap_or("a tool").to_string();
                        if let Some(id) = b.get("id").and_then(Value::as_str) {
                            tool_names.insert(id.to_string(), name.clone());
                        }
                        let summary = input_summary(&name, b.get("input").unwrap_or(&Value::Null));
                        if let Some(e) = push(&mut out, EntryKind::Action, ts, &summary, Some(name), false) {
                            e.tool_use_id = b.get("id").and_then(Value::as_str).map(str::to_string);
                        }
                    }
                    Some("tool_result") => {
                        let tool = b.get("tool_use_id").and_then(Value::as_str).and_then(|id| tool_names.get(id).cloned());
                        let is_error = b.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                        if let Some(e) = push(&mut out, EntryKind::Result, ts, &result_text(b.get("content")), tool, is_error) {
                            e.tool_use_id = b.get("tool_use_id").and_then(Value::as_str).map(str::to_string);
                        }
                    }
                    // redacted_thinking, image, and anything new: not shown.
                    _ => {}
                }
            }
        }
        _ => return None,
    }
    Some(out)
}

/// Parses JSONL text. `window` (start, end in ms) limits entries to the task's time when any
/// entry falls inside it; otherwise only the latest request onward is kept and `windowed` stays false.
pub fn parse_jsonl(data: &str, window: Option<(i64, i64)>) -> Parsed {
    let mut p = Parsed::default();
    let mut all = vec![];
    let mut tool_names = std::collections::HashMap::new();
    for line in data.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        p.total_lines += 1;
        let parsed = serde_json::from_str::<Value>(line).ok().and_then(|v| parse_line(&v, &mut tool_names));
        match parsed {
            Some(entries) => all.extend(entries),
            None => p.skipped_lines += 1,
        }
    }
    if let Some((start, end)) = window {
        let inside = |e: &Entry| e.ts.is_some_and(|t| t >= start && t <= end);
        if all.iter().any(inside) {
            // Untimed entries stay only when they sit between timed ones inside the window.
            let first = all.iter().position(inside).unwrap_or(0);
            let last = all.iter().rposition(inside).unwrap_or(all.len().saturating_sub(1));
            all = all.drain(first..=last).filter(|e| e.ts.is_none() || inside(e)).collect();
            p.windowed = true;
        } else if let Some(last_you) = all.iter().rposition(|e| e.kind == EntryKind::You && e.ts.is_none_or(|t| t <= end)) {
            // Nothing inside the window: show the latest request and what followed, never a
            // mix of several requests.
            all.drain(..last_you);
        }
    }
    link_notes(&mut all);
    if all.len() > MAX_ENTRIES {
        all.drain(..all.len() - MAX_ENTRIES);
        p.capped = true;
    }
    p.entries = all;
    p
}

/// Reads the newest `MAX_READ_BYTES` of a record. Returns (text, started_mid_file).
pub fn read_tail(path: &Path) -> std::io::Result<(String, bool)> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(MAX_READ_BYTES);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.take(MAX_READ_BYTES).read_to_end(&mut buf)?;
    let mut text = String::from_utf8_lossy(&buf).into_owned();
    if start > 0 {
        // The first line is probably cut in half.
        text = text.split_once('\n').map(|(_, rest)| rest.to_string()).unwrap_or_default();
    }
    Ok((text, start > 0))
}

fn is_safe_session_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Looks for `<root>/projects/<any>/<session_id>.jsonl`, for tasks recorded before the path was saved.
fn find_by_session(session_id: &str, roots: &[PathBuf]) -> Option<PathBuf> {
    if !is_safe_session_id(session_id) {
        return None;
    }
    for r in roots {
        let Ok(dirs) = std::fs::read_dir(r.join("projects")) else { continue };
        for d in dirs.flatten() {
            let cand = d.path().join(format!("{session_id}.jsonl"));
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

/// The record path the hooks told us about for this task (newest first), if any.
pub fn transcript_path_from_events(events: &[AgentEvent]) -> Option<String> {
    events
        .iter()
        .rev()
        .filter(|e| e.connector_name == crate::connectors::claude_code::NAME)
        .find_map(|e| e.payload_redacted.get("transcript_path").and_then(Value::as_str).map(str::to_string))
}

/// Everything the report's "Conversation" section needs, as JSON.
/// `status` is one of: `ok`, `empty`, `not_claude`, `no_record`, `missing`, `unsafe`, `unreadable`.
pub fn for_task(task: &TaskDefinition, run: &VerificationRun, events: &[AgentEvent], roots: &[PathBuf]) -> Value {
    let claude_events = events.iter().any(|e| e.connector_name == crate::connectors::claude_code::NAME);
    let session_id = task
        .session_id
        .clone()
        .or_else(|| events.iter().rev().filter(|e| e.connector_name == crate::connectors::claude_code::NAME).find_map(|e| e.session_id.clone()));
    let base = |status: &str, message: &str, path: Option<&str>| {
        json!({"status": status, "message": message, "path": path, "session_id": session_id, "entries": [], "skipped_lines": 0, "total_lines": 0, "windowed": false, "capped": false, "partial_read": false})
    };
    if task.connector == crate::connectors::cursor::NAME && !claude_events {
        return base("not_claude", "Cursor doesn't share its conversation with Houndbit, so only what the agent did is shown, not what it wrote.", None);
    }
    if task.connector != crate::connectors::claude_code::NAME && !claude_events {
        return base("not_claude", "Conversations show up for Claude Code tasks. The regular Claude chat doesn't share its conversation.", None);
    }
    let recorded = transcript_path_from_events(events);
    let path = match &recorded {
        Some(raw) => match safe_transcript_path(raw, roots) {
            Some(p) => p,
            None if !Path::new(raw).exists() => {
                return base("missing", "Claude Code's record of this conversation isn't where it used to be. It may have been moved, renamed or tidied away (Claude Code clears out old records after a while).", Some(raw));
            }
            None => return base("unsafe", "The conversation record isn't inside your Claude folder, so I left it alone.", Some(raw)),
        },
        None => match session_id.as_deref().and_then(|s| find_by_session(s, roots)).and_then(|p| safe_transcript_path(&p.to_string_lossy(), roots)) {
            Some(p) => p,
            None => {
                return base("no_record", "Claude Code didn't share a conversation record for this task. Conversations show up when you use Claude Code (the Code tab in the Claude desktop app, or claude in a terminal). The regular Claude chat doesn't share its conversation.", None);
            }
        },
    };
    let shown = path.to_string_lossy().trim_start_matches(r"\\?\").to_string();
    let (text, partial) = match read_tail(&path) {
        Ok(x) => x,
        Err(e) => return base("unreadable", &format!("I found Claude Code's record but couldn't open it ({e})."), Some(&shown)),
    };
    let (start, end) = crate::event::task_window(task.created_at, run.started_at, run.finished_at, events);
    let parsed = parse_jsonl(&text, Some((start, end)));
    let status = if parsed.entries.is_empty() { "empty" } else { "ok" };
    let message = if parsed.entries.is_empty() { "Claude Code's record is there, but it has nothing I can show for this task." } else { "" };
    let mut v = base(status, message, Some(&shown));
    // Match each action to the start/finish records the hooks sent, by tool-call id.
    let hook_ids: std::collections::HashSet<&str> = events.iter().filter_map(|e| e.tool_call_id.as_deref()).collect();
    let mut entries = serde_json::to_value(&parsed.entries).unwrap_or(Value::Null);
    let (mut actions, mut with_note, mut matched) = (0usize, 0usize, 0usize);
    if let Some(arr) = entries.as_array_mut() {
        for e in arr.iter_mut().filter(|e| e.get("kind").and_then(Value::as_str) == Some("action")) {
            actions += 1;
            if e.get("note_before").is_some() {
                with_note += 1;
            }
            let hit = e.get("tool_use_id").and_then(Value::as_str).is_some_and(|id| hook_ids.contains(id));
            if hit {
                matched += 1;
            }
            e["matched_hook"] = json!(hit);
        }
    }
    v["entries"] = entries;
    v["actions"] = json!(actions);
    v["actions_with_note"] = json!(with_note);
    v["hook_matched"] = json!(matched);
    v["skipped_lines"] = json!(parsed.skipped_lines);
    v["total_lines"] = json!(parsed.total_lines);
    v["windowed"] = json!(parsed.windowed);
    v["capped"] = json!(parsed.capped);
    v["partial_read"] = json!(partial);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = include_str!("../tests/fixtures/claude_transcript.jsonl");

    #[test]
    fn parses_every_kind_and_skips_the_rest() {
        let p = parse_jsonl(SAMPLE, None);
        let kinds: Vec<_> = p.entries.iter().map(|e| e.kind.clone()).collect();
        assert_eq!(
            kinds,
            vec![EntryKind::You, EntryKind::Thinking, EntryKind::Claude, EntryKind::Action, EntryKind::Result, EntryKind::Action, EntryKind::Result, EntryKind::Claude]
        );
        assert_eq!(p.entries[0].text, "create a file named letitgo.txt with A to Z");
        assert!(p.entries[1].text.starts_with("The user wants"));
        assert_eq!(p.entries[3].tool.as_deref(), Some("Write"));
        assert!(p.entries[3].text.ends_with("letitgo.txt"));
        assert_eq!(p.entries[4].tool.as_deref(), Some("Write"), "result is matched to its action");
        assert!(!p.entries[4].is_error);
        assert_eq!(p.entries[5].tool.as_deref(), Some("Bash"));
        assert_eq!(p.entries[5].text, "type letitgo.txt");
        assert!(p.entries[6].is_error);
        assert_eq!(p.entries[6].text, "The system cannot find the file specified.");
        // summary, unknown type, broken JSON, meta line, side chat, missing message.
        assert_eq!(p.skipped_lines, 6);
        assert_eq!(p.total_lines, 14);
        assert!(!p.windowed);
        assert_eq!(p.entries[0].ts, parse_ts("2026-11-05T14:00:01.000Z"));
    }

    #[test]
    fn actions_are_linked_to_the_request_and_the_note_before_them() {
        let data = [
            json!({"type": "user", "message": {"content": "make a PDF of GDP by country"}}),
            json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "I'll use GDP per capita because totals favour big countries."}, {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "python make.py"}}, {"type": "tool_use", "id": "toolu_2", "name": "Read", "input": {"file_path": "a.csv"}}]}}),
            json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok"}]}}),
            json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "toolu_3", "name": "Bash", "input": {"command": "ls"}}]}}),
        ]
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        let p = parse_jsonl(&data, None);
        let acts: Vec<&Entry> = p.entries.iter().filter(|e| e.kind == EntryKind::Action).collect();
        assert_eq!(acts.len(), 3);
        assert_eq!(acts[0].tool_use_id.as_deref(), Some("toolu_1"));
        assert_eq!(acts[0].instruction.as_deref(), Some("make a PDF of GDP by country"));
        assert!(acts[0].note_before.as_deref().unwrap().starts_with("I'll use GDP per capita"));
        assert_eq!(acts[1].note_before, acts[0].note_before, "actions started together share the note");
        assert_eq!(acts[2].note_before, None, "nothing written after the last result");
        assert_eq!(acts[2].instruction.as_deref(), Some("make a PDF of GDP by country"));
        let res = p.entries.iter().find(|e| e.kind == EntryKind::Result).unwrap();
        assert_eq!(res.tool_use_id.as_deref(), Some("toolu_1"));
    }

    #[test]
    fn secrets_are_scrubbed_from_the_conversation() {
        let p = parse_jsonl(SAMPLE, None);
        let last = &p.entries.last().unwrap().text;
        assert!(!last.contains("sk-ant-"), "{last}");
        assert!(last.contains("[REDACTED]"));
    }

    #[test]
    fn limits_to_the_task_time_and_falls_back_when_nothing_matches() {
        let start = parse_ts("2026-11-05T14:00:04Z").unwrap();
        let end = parse_ts("2026-11-05T14:00:06Z").unwrap();
        let p = parse_jsonl(SAMPLE, Some((start, end)));
        assert!(p.windowed);
        assert!(p.entries.iter().all(|e| e.ts.is_some_and(|t| t >= start && t <= end)));
        assert!(p.entries.iter().any(|e| e.kind == EntryKind::Action));
        assert!(!p.entries.iter().any(|e| e.kind == EntryKind::You));

        let far = parse_ts("2030-01-01T00:00:00Z").unwrap();
        let p = parse_jsonl(SAMPLE, Some((far, far + 1)));
        assert!(!p.windowed, "nothing inside the window: show everything rather than nothing");
        assert_eq!(p.entries.len(), 8);
    }

    #[test]
    fn falls_back_to_the_latest_request_not_a_mix() {
        let line = |role: &str, ts: &str, text: &str| {
            json!({"type": role, "timestamp": ts, "message": {"role": role, "content": [{"type": "text", "text": text}]}}).to_string()
        };
        let data = [
            line("user", "2026-11-05T10:00:00Z", "create twofortea.txt"),
            line("assistant", "2026-11-05T10:00:05Z", "Created it."),
            line("user", "2026-11-05T11:00:00Z", "erase twofortea.txt"),
            line("assistant", "2026-11-05T11:00:05Z", "Deleted it."),
        ]
        .join("\n");
        let far = parse_ts("2026-11-05T12:00:00Z").unwrap();
        let p = parse_jsonl(&data, Some((far, far + 1000)));
        let texts: Vec<_> = p.entries.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, vec!["erase twofortea.txt", "Deleted it."]);

        let start = parse_ts("2026-11-05T10:59:58Z").unwrap();
        let p = parse_jsonl(&data, Some((start, start + 60_000)));
        assert!(p.windowed);
        assert_eq!(p.entries.len(), 2);
        assert_eq!(p.entries[0].text, "erase twofortea.txt");
    }

    #[test]
    fn long_text_is_cut_and_garbage_never_panics() {
        let long = "x".repeat(MAX_TEXT_CHARS + 50);
        let line = json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": long}]}}).to_string();
        let p = parse_jsonl(&line, None);
        assert!(p.entries[0].cut);
        assert_eq!(p.entries[0].text.chars().count(), MAX_TEXT_CHARS);

        for junk in ["", "\n\n", "null", "[]", "{\"type\":5}", "{\"type\":\"user\",\"message\":{\"content\":7}}", "\u{0}\u{1}", "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\"}]}}"] {
            let _ = parse_jsonl(junk, Some((0, 1)));
        }
        let many: String = (0..MAX_ENTRIES + 10).map(|i| json!({"type": "user", "message": {"content": format!("m{i}")}}).to_string() + "\n").collect();
        let p = parse_jsonl(&many, None);
        assert!(p.capped);
        assert_eq!(p.entries.len(), MAX_ENTRIES);
        assert_eq!(p.entries.last().unwrap().text, format!("m{}", MAX_ENTRIES + 9), "the newest entries are kept");
    }

    #[test]
    fn reads_only_the_tail_of_a_big_record() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("big.jsonl");
        let line = json!({"type": "user", "message": {"content": "y".repeat(1000)}}).to_string() + "\n";
        let n = (MAX_READ_BYTES as usize / line.len()) + 50;
        std::fs::write(&f, line.repeat(n)).unwrap();
        let (text, partial) = read_tail(&f).unwrap();
        assert!(partial);
        assert!(text.len() as u64 <= MAX_READ_BYTES);
        let p = parse_jsonl(&text, None);
        assert_eq!(p.skipped_lines, 0, "the half line at the cut was dropped");
    }

    #[test]
    fn parses_times() {
        assert_eq!(parse_ts("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_ts("1970-01-01T00:00:01.5Z"), Some(1500));
        assert_eq!(parse_ts("2026-11-05T14:00:00+01:00"), parse_ts("2026-11-05T13:00:00Z"));
        assert_eq!(parse_ts("2026-11-05T14:00:00.123456Z").map(|t| t % 1000), Some(123));
        assert_eq!(parse_ts("yesterday"), None);
        assert_eq!(parse_ts("2026-13-05T14:00:00Z"), None);
    }

    #[test]
    fn path_safety() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("home").join(".claude");
        let proj = root.join("projects").join("C--Users-Greg");
        std::fs::create_dir_all(&proj).unwrap();
        let good = proj.join("abc.jsonl");
        std::fs::write(&good, "{}").unwrap();
        let outside = t.path().join("elsewhere.jsonl");
        std::fs::write(&outside, "{}").unwrap();
        let not_jsonl = proj.join("notes.txt");
        std::fs::write(&not_jsonl, "{}").unwrap();
        let roots = vec![root.clone()];
        let s = |p: &Path| p.to_string_lossy().into_owned();

        assert!(safe_transcript_path(&s(&good), &roots).is_some());
        assert!(safe_transcript_path(&s(&outside), &roots).is_none(), "outside the Claude folder");
        assert!(safe_transcript_path(&s(&not_jsonl), &roots).is_none(), "not a .jsonl file");
        assert!(safe_transcript_path(&s(&proj.join("..").join("..").join("..").join("elsewhere.jsonl")), &roots).is_none(), ".. steps");
        assert!(safe_transcript_path("abc.jsonl", &roots).is_none(), "relative path");
        assert!(safe_transcript_path(&s(&proj.join("gone.jsonl")), &roots).is_none(), "missing file");
        assert!(safe_transcript_path(&s(&proj), &roots).is_none(), "a folder");
        #[cfg(unix)]
        {
            let link = proj.join("sneaky.jsonl");
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            assert!(safe_transcript_path(&s(&link), &roots).is_none(), "a link that points outside");
        }

        assert!(plausible_transcript_path(&s(&good)).is_some());
        assert!(plausible_transcript_path(&s(&outside)).is_none());
        assert!(plausible_transcript_path(&s(&not_jsonl)).is_none());
        assert!(plausible_transcript_path("relative/.claude/x.jsonl").is_none());
        assert!(plausible_transcript_path(&s(&root.join("..").join("x.jsonl"))).is_none());

        assert!(is_safe_session_id("2f6c-AB_9"));
        assert!(!is_safe_session_id("../../etc"));
        assert_eq!(find_by_session("abc", &roots).as_deref(), Some(good.as_path()));
        assert_eq!(find_by_session("..\\abc", &roots), None);
    }
}
