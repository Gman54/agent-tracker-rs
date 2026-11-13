//! Chat recorder + folder guard: when the model asks for a tool call that would change a guarded
//! folder, the chat app gets a refusal instead and the guard log says so. Its own test binary
//! (own process), because it points AGENTTRACKER_HOME at a test folder.
use agenttracker_core::db::Db;
use agenttracker_runtime::relay::{self, guard_reply, parse_response, RelayConfig};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;

const GUARDED: &str = "/home/greg/Test";

fn guard_file(home: &Path, mode: &str, enabled: bool) {
    std::fs::create_dir_all(home).unwrap();
    std::fs::write(home.join("guard.json"), json!({"enabled": enabled, "folders": [{"path": GUARDED, "mode": mode}]}).to_string()).unwrap();
}

fn chat_reply(tool: &str, args: Value) -> Vec<u8> {
    json!({"id": "chatcmpl-1", "model": "qwen3-8b", "choices": [{"index": 0, "message": {"role": "assistant", "content": "",
        "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": tool, "arguments": args.to_string()}}]}, "finish_reason": "tool_calls"}]})
    .to_string()
    .into_bytes()
}

fn chat_stream(tool: &str, args: Value) -> Vec<u8> {
    let a = args.to_string();
    let (p1, p2) = a.split_at(a.len() / 2);
    let mut s = String::new();
    s.push_str(&format!("data: {}\n\n", json!({"id": "c1", "model": "qwen3-8b", "choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "call_1", "function": {"name": tool, "arguments": p1}}]}}]})));
    s.push_str(&format!("data: {}\n\n", json!({"id": "c1", "model": "qwen3-8b", "choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "function": {"arguments": p2}}]}}]})));
    s.push_str(&format!("data: {}\n\n", json!({"id": "c1", "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]})));
    s.push_str("data: [DONE]\n\n");
    s.into_bytes()
}

#[test]
fn guard_reply_replaces_tool_calls_into_guarded_folders() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().join("home");
    guard_file(&home, "block", true);
    let write = json!({"path": format!("{GUARDED}/notes.txt"), "content": "x"});

    // Plain JSON reply.
    let out = guard_reply(&home, "chat_completions", Some("application/json"), &chat_reply("write_file", write.clone())).expect("stopped");
    let p = parse_response("chat_completions", Some("application/json"), &out);
    assert!(p.tool_calls.is_empty(), "{p:?}");
    assert!(p.reply.contains("Houndbit Protect stopped this") && p.reply.contains(GUARDED), "{}", p.reply);
    assert_eq!(p.finish_reason.as_deref(), Some("stop"));

    // Streamed reply, tool call split across chunks.
    let out = guard_reply(&home, "chat_completions", Some("text/event-stream"), &chat_stream("write_file", write.clone())).expect("stopped");
    let text = String::from_utf8(out.clone()).unwrap();
    assert!(text.ends_with("data: [DONE]\n\n"), "{text}");
    let p = parse_response("chat_completions", Some("text/event-stream"), &out);
    assert!(p.tool_calls.is_empty() && p.reply.contains("Houndbit Protect") && p.end_marker, "{p:?}");

    // Ollama.
    let ollama = json!({"model": "llama3", "message": {"role": "assistant", "content": "", "tool_calls": [{"function": {"name": "run_command", "arguments": {"command": format!("rm -rf {GUARDED}")}}}]}, "done": true}).to_string();
    let out = guard_reply(&home, "ollama_chat", Some("application/json"), ollama.as_bytes()).expect("stopped");
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert!(v["message"]["content"].as_str().unwrap().contains("Houndbit Protect"));
    assert!(v["message"].get("tool_calls").is_none());

    // Left alone: other folders, tools that only read, no tool calls at all.
    assert!(guard_reply(&home, "chat_completions", None, &chat_reply("write_file", json!({"path": "/home/greg/Other/a.txt"}))).is_none());
    assert!(guard_reply(&home, "chat_completions", None, &chat_reply("read_file", json!({"path": format!("{GUARDED}/a.txt")}))).is_none());
    assert!(guard_reply(&home, "chat_completions", None, br#"{"choices":[{"message":{"content":"hi"}}]}"#).is_none());

    let log = tracker_common::guard::recent_log(&home, 10);
    assert_eq!(log.len(), 3);
    assert!(log.iter().all(|e| e["app"] == "Chat recorder" && e["decision"] == "blocked"));
}

#[test]
fn ask_folders_are_stopped_and_the_switch_turns_it_off() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().join("home");
    guard_file(&home, "ask", true);
    let reply = chat_reply("write_file", json!({"path": format!("{GUARDED}/a.txt")}));
    assert!(guard_reply(&home, "chat_completions", None, &reply).is_some());
    assert!(tracker_common::guard::recent_log(&home, 1)[0]["why"].as_str().unwrap().contains("can't wait"));
    guard_file(&home, "block", false);
    assert!(guard_reply(&home, "chat_completions", None, &reply).is_none());
}

/// A model server that always asks to write into the guarded folder (or somewhere harmless when
/// the request says "harmless"), as plain JSON or as a stream.
fn fake_server() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { continue };
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut first = String::new();
            r.read_line(&mut first).unwrap();
            let mut len = 0usize;
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap() == 0 || line.trim().is_empty() {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; len];
            r.read_exact(&mut body).unwrap();
            let v: Value = serde_json::from_slice(&body).unwrap();
            let dir = if v.to_string().contains("harmless") { "/home/greg/Other" } else { GUARDED };
            let args = json!({"path": format!("{dir}/sigmadelta.txt"), "content": "2 3 5"});
            let (ct, out) = if v["stream"].as_bool() == Some(true) { ("text/event-stream", chat_stream("write_file", args)) } else { ("application/json", chat_reply("write_file", args)) };
            let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", out.len());
            let _ = s.write_all(&out);
        }
    });
    port
}

#[test]
fn relay_stops_tool_calls_into_guarded_folders() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().join("home");
    guard_file(&home, "block", true);
    std::env::set_var("AGENTTRACKER_HOME", &home);
    let up = fake_server();
    let db = Db::open_in_memory().unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let port = rt.block_on(relay::start(db.clone(), RelayConfig { enabled: true, port: 0, upstream: format!("http://127.0.0.1:{up}/v1") })).unwrap();
    let url = format!("http://127.0.0.1:{port}/v1/chat/completions");
    let tools = json!([{"type": "function", "function": {"name": "write_file", "parameters": {"type": "object"}}}]);

    let req = json!({"model": "qwen3-8b", "messages": [{"role": "user", "content": "Write primes to my Test folder"}], "tools": tools});
    let v: Value = ureq::post(&url).send_json(req.clone()).unwrap().into_json().unwrap();
    assert!(v["choices"][0]["message"].get("tool_calls").is_none(), "{v}");
    assert!(v["choices"][0]["message"]["content"].as_str().unwrap().contains("Houndbit Protect stopped this"));

    let mut sreq = req.clone();
    sreq["stream"] = json!(true);
    let resp = ureq::post(&url).send_json(sreq).unwrap();
    assert_eq!(resp.header("content-type"), Some("text/event-stream"));
    let sse = resp.into_string().unwrap();
    assert!(sse.contains("Houndbit Protect stopped this") && !sse.contains("tool_calls") && sse.ends_with("data: [DONE]\n\n"), "{sse}");

    // Harmless tool calls, and requests without tools, pass through unchanged.
    let mut ok = req.clone();
    ok["messages"][0]["content"] = json!("harmless");
    let v: Value = ureq::post(&url).send_json(ok).unwrap().into_json().unwrap();
    assert_eq!(v["choices"][0]["message"]["tool_calls"][0]["function"]["name"], "write_file");
    let mut no_tools = req.clone();
    no_tools.as_object_mut().unwrap().remove("tools");
    let v: Value = ureq::post(&url).send_json(no_tools).unwrap().into_json().unwrap();
    assert_eq!(v["choices"][0]["message"]["tool_calls"][0]["function"]["name"], "write_file", "only checked when the model was offered tools");

    let log = tracker_common::guard::recent_log(&home, 10);
    assert_eq!(log.len(), 2, "{log:?}");
    assert!(log.iter().all(|e| e["app"] == "Chat recorder"));
    relay::stop();
}
