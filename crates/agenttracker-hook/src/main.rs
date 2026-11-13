//! agenttracker-hook — invoked by AI agents on lifecycle events.
//!
//!   agenttracker-hook --connector claude_code --event PreToolUse [--payload '<json>'] [--no-stdin]
//!
//! 1. Reads the agent's event JSON from stdin (or --payload).
//! 2. Reads the collector port + secret from ~/.agenttracker/collector.json.
//! 3. POSTs an HMAC-signed envelope to http://127.0.0.1:{port}/ingest (200 ms timeouts).
//! 4. If the collector is unreachable, appends the (redacted) envelope to
//!    ~/.agenttracker/fallback_queue.jsonl.
//! 5. Exits 0 and prints nothing, so it never alters agent work, with one exception:
//! 6. Folder guard (Houndbit Protect): on Claude Code `PreToolUse`, if the request would change a
//!    folder listed in `guard.json`, it stops the tool (exit code 2 plus a reason on stderr, which
//!    Claude Code shows to the model) or, for "ask" folders, waits for the owner's OK in Houndbit.
//!    No answer in time means stop. See `tracker_common::guard`.
//! 7. Same guard for Cursor's `preToolUse`, `beforeShellExecution` and `beforeMCPExecution`: these
//!    always print Cursor's JSON answer (`{"permission":"allow"}` or `"deny"` with a reason).
//!
//! On Windows this is a GUI-subsystem program so no console window flashes each time an
//! agent runs a hook. Piped stdin/stdout from the agent still work as normal.
#![cfg_attr(windows, windows_subsystem = "windows")]
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_millis(200);
const MAX_STDIN: u64 = 4 * 1024 * 1024;
/// How long an "ask" waits for the owner. Claude Code's timeout for this hook is set higher
/// (see `claude_code::PRE_TOOL_TIMEOUT_SECS`) so the wait ends here, not by Claude giving up.
const ASK_WAIT: Duration = Duration::from_secs(90);
/// Cursor runs two hooks for one shell command; an OK given in the first covers the second.
const RECENT_OK_MS: i64 = 2 * 60_000;

struct Args {
    connector: String,
    event: String,
    payload: Option<String>,
    no_stdin: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args { connector: "unknown".into(), event: "unknown".into(), payload: None, no_stdin: false };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--version" | "-V" => return Err(format!("agenttracker-hook {}", env!("CARGO_PKG_VERSION"))),
            "--connector" => a.connector = it.next().unwrap_or_default(),
            "--event" => a.event = it.next().unwrap_or_default(),
            "--payload" => a.payload = it.next(),
            "--no-stdin" => a.no_stdin = true,
            "--at-owner" => {
                it.next();
            }
            _ => {}
        }
    }
    Ok(a)
}

fn home() -> PathBuf {
    if let Some(h) = std::env::var_os("AGENTTRACKER_HOME").filter(|v| !v.is_empty()) {
        return h.into();
    }
    std::env::var_os("USERPROFILE")
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".agenttracker")
}

fn read_payload(a: &Args) -> Value {
    let text = if let Some(p) = &a.payload {
        p.clone()
    } else if a.no_stdin {
        String::new()
    } else {
        let mut s = String::new();
        let _ = std::io::stdin().take(MAX_STDIN).read_to_string(&mut s);
        s
    };
    if text.trim().is_empty() {
        return json!({});
    }
    serde_json::from_str(&text).unwrap_or_else(|_| json!({"unparsed_input": text.chars().take(10_000).collect::<String>()}))
}

/// Minimal HTTP/1.1 POST over a raw socket (avoids pulling an HTTP client into a hot-path binary).
fn post(port: u16, secret: &str, body: &[u8]) -> std::io::Result<u16> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut s = TcpStream::connect_timeout(&addr, TIMEOUT)?;
    s.set_read_timeout(Some(TIMEOUT))?;
    s.set_write_timeout(Some(TIMEOUT))?;
    let ts = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)).to_string();
    let sig = tracker_common::security::sign_body(secret, &ts, body);
    let head = format!(
        "POST /ingest HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nX-AT-Timestamp: {ts}\r\nX-AT-Signature: {sig}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes())?;
    s.write_all(body)?;
    let mut buf = [0u8; 64];
    let n = s.read(&mut buf)?;
    let line = String::from_utf8_lossy(&buf[..n]);
    Ok(line.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0))
}

fn fallback(home: &std::path::Path, envelope: &Value) {
    let redacted = tracker_common::redaction::redact_deep(envelope);
    let _ = std::fs::create_dir_all(home);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(home.join("fallback_queue.jsonl")) {
        let _ = writeln!(f, "{redacted}");
    }
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(version) => {
            // Never panic if there is no stdout (e.g. started without a console).
            let _ = writeln!(std::io::stdout(), "{version}");
            return;
        }
    };
    let payload = read_payload(&args);
    let hook_ts_ns = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as i64).unwrap_or(0);
    let envelope = json!({
        "connector": args.connector,
        "hook_event": args.event,
        "hook_ts_ns": hook_ts_ns,
        "hook_version": env!("CARGO_PKG_VERSION"),
        "payload": payload,
    });
    let home = home();
    let body = envelope.to_string();
    let delivered = std::fs::read_to_string(home.join("collector.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|d| Some((d.get("port")?.as_u64()? as u16, d.get("secret")?.as_str()?.to_string())))
        .map(|(port, secret)| matches!(post(port, &secret, body.as_bytes()), Ok(c) if (200..300).contains(&c)))
        .unwrap_or(false);
    if !delivered {
        fallback(&home, &envelope);
    }
    let event = envelope["payload"].get("hook_event_name").and_then(Value::as_str).unwrap_or(&args.event).to_string();
    if args.connector == "claude_code" && event == "PreToolUse" {
        let p = &envelope["payload"];
        let tool = p.get("tool_name").and_then(Value::as_str).unwrap_or("unknown");
        let input = p.get("tool_input").cloned().unwrap_or(Value::Null);
        if let Some(reason) = guard_decision(&home, "Claude Code", tool, &input, p.get("cwd").and_then(Value::as_str)) {
            let _ = writeln!(std::io::stderr(), "{reason}");
            std::process::exit(2);
        }
    }
    if args.connector == "cursor" {
        if let Some((tool, input, cwd)) = cursor_request(&event, &envelope["payload"]) {
            // Cursor's permission hooks must always answer with JSON; anything else blocks the action.
            let out = match guard_decision(&home, "Cursor", &tool, &input, cwd.as_deref()) {
                Some(reason) => json!({"permission": "deny", "user_message": reason, "agent_message": reason}),
                None => json!({"permission": "allow"}),
            };
            let _ = writeln!(std::io::stdout(), "{out}");
        }
    }
}

/// Turns Cursor's three deny-capable hooks into one tool request (name, input, folder).
/// `None` for the hooks that only report.
fn cursor_request(event: &str, p: &Value) -> Option<(String, Value, Option<String>)> {
    let cwd = p.get("cwd").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
    match event {
        "preToolUse" => {
            let tool = p.get("tool_name").and_then(Value::as_str).unwrap_or("unknown").to_string();
            Some((tool, p.get("tool_input").cloned().unwrap_or(Value::Null), cwd))
        }
        "beforeShellExecution" => {
            // Same shape as preToolUse's Shell tool, so an OK given in one isn't asked again in the other.
            let command = p.get("command").and_then(Value::as_str).unwrap_or("");
            let mut input = json!({"command": command});
            if let Some(c) = &cwd {
                input["working_directory"] = json!(c);
            }
            Some(("Shell".into(), input, cwd))
        }
        "beforeMCPExecution" => {
            let tool = format!("MCP:{}", p.get("tool_name").and_then(Value::as_str).unwrap_or("unknown"));
            let input = match p.get("tool_input") {
                Some(Value::String(s)) => serde_json::from_str(s).unwrap_or_else(|_| json!({"input": s})),
                Some(v) => v.clone(),
                None => Value::Null,
            };
            Some((tool, input, cwd))
        }
        _ => None,
    }
}

/// `Some(reason)` when the tool must be stopped. `app` is shown to the owner ("Claude Code", "Cursor").
fn guard_decision(home: &std::path::Path, app: &str, tool: &str, input: &Value, cwd: Option<&str>) -> Option<String> {
    use tracker_common::guard;
    let list = match guard::load_strict(home) {
        Ok(l) => l,
        Err(e) => {
            if !guard::can_change(tool) {
                return None;
            }
            let why = format!("Houndbit Protect couldn't read its list of guarded folders ({e}), so it stopped this to be safe. Open Houndbit > Protect to fix it.");
            guard::log(home, guard::log_entry(app, tool, "", "", "blocked", &why));
            return Some(why);
        }
    };
    let guard::Verdict::Guarded { folder, mode, target } = guard::check(&list, tool, input, cwd) else { return None };
    if mode == "ask" {
        let key = format!("{app}|{tool}|{folder}|{target}");
        if guard::recently_ok(home, &key, RECENT_OK_MS) {
            return None;
        }
        let id = format!("{}-{}", tracker_common::now_ms(), std::process::id());
        let req = serde_json::json!({"id": id, "ts": tracker_common::now_ms(), "app": app, "tool": tool, "target": target, "folder": folder, "cwd": cwd});
        match guard::ask_and_wait(home, &id, &req, ASK_WAIT) {
            Some(true) => {
                guard::remember_ok(home, &key);
                guard::log(home, guard::log_entry(app, tool, &target, &folder, "allowed", "You said OK in Houndbit."));
                return None;
            }
            Some(false) => {
                guard::log(home, guard::log_entry(app, tool, &target, &folder, "blocked", "You said no in Houndbit."));
                return Some(format!("Houndbit Protect: the owner said no to changing {folder} ({tool}: {target}). Don't try another way to change files in that folder; ask the owner instead."));
            }
            None => {
                guard::log(home, guard::log_entry(app, tool, &target, &folder, "blocked", "No answer in time."));
                return Some(format!("Houndbit Protect: {folder} is guarded and the owner didn't answer in time, so this was stopped ({tool}: {target}). Ask the owner to approve it in Houndbit > Protect, then try again."));
            }
        }
    }
    guard::log(home, guard::log_entry(app, tool, &target, &folder, "blocked", "This folder is set to always block."));
    Some(format!("Houndbit Protect: {folder} is guarded, so AI helpers can't change it ({tool}: {target}). Don't try another way to change files in that folder; ask the owner to change the setting in Houndbit > Protect if this is wanted."))
}
