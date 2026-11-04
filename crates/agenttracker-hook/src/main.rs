//! agenttracker-hook — invoked by AI agents on lifecycle events.
//!
//!   agenttracker-hook --connector claude_code --event PreToolUse [--payload '<json>'] [--no-stdin]
//!
//! 1. Reads the agent's event JSON from stdin (or --payload).
//! 2. Reads the collector port + secret from ~/.agenttracker/collector.json.
//! 3. POSTs an HMAC-signed envelope to http://127.0.0.1:{port}/ingest (200 ms timeouts).
//! 4. If the collector is unreachable, appends the (redacted) envelope to
//!    ~/.agenttracker/fallback_queue.jsonl.
//! 5. Always exits 0 and prints nothing to stdout, so it never blocks or alters agent work.
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_millis(200);
const MAX_STDIN: u64 = 4 * 1024 * 1024;

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
            println!("{version}");
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
    // Exit 0 regardless: the hook must never block the agent.
}
