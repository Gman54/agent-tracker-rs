//! Chat recorder (relay): passes calls through unchanged and records them.
use agenttracker_core::db::Db;
use agenttracker_runtime::relay::{self, parse_response, RelayConfig};
use agenttracker_runtime::store;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};

/// The relay is one per process (start() replaces a running one), so tests that start it take turns.
static RELAY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A fake model server. `/v1/chat/completions` answers plain JSON with reasoning_content, or an SSE
/// stream with `<think>` tags when the request has "stream": true. `/v1/models` lists one model.
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
                if r.read_line(&mut line).unwrap() == 0 || line.trim().is_empty() { break; }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") { len = v.trim().parse().unwrap(); }
            }
            let mut body = vec![0; len];
            r.read_exact(&mut body).unwrap();
            let (ct, out) = if first.contains("/v1/models") {
                ("application/json", json!({"data": [{"id": "qwen3-8b"}]}).to_string())
            } else {
                let v: Value = serde_json::from_slice(&body).unwrap();
                if v["stream"].as_bool() == Some(true) {
                    let chunks = ["<think>The user", " wants a haiku.</think>", "Autumn ", "leaves fall"];
                    let mut sse = String::new();
                    for c in chunks { sse.push_str(&format!("data: {}\n\n", json!({"choices": [{"delta": {"content": c}}]}))); }
                    sse.push_str(&format!("data: {}\n\n", json!({"choices": [{"delta": {}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 12, "completion_tokens": 9}})));
                    sse.push_str("data: [DONE]\n\n");
                    ("text/event-stream", sse)
                } else {
                    ("application/json", json!({"model": "qwen3-8b", "choices": [{"message": {"role": "assistant", "content": "Paris.", "reasoning_content": "France's capital is Paris.",
                        "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "lookup", "arguments": "{\"q\":\"paris\"}"}}]}, "finish_reason": "tool_calls"}],
                        "usage": {"prompt_tokens": 20, "completion_tokens": 5}}).to_string())
                }
            };
            let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}", out.len());
        }
    });
    port
}

fn rows(db: &Db) -> Vec<Value> {
    // Two calls can start in the same millisecond and are recorded in the background, so break
    // ties by the plain (not streamed) call first; the tests below send it first.
    store::rows(&db.conn(), "SELECT * FROM relay_calls ORDER BY started_at, streamed", &[]).unwrap()
}

fn wait_rows(db: &Db, n: usize) -> Vec<Value> {
    for _ in 0..100 {
        let r = rows(db);
        if r.len() >= n { return r; }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    rows(db)
}

/// Sends one POST over a plain TCP socket with exactly one Host header and returns everything
/// that came back (status line, headers and body), or a description of the socket error.
fn raw_post(port: u16, host: &str, body: &str) -> String {
    use std::net::TcpStream;
    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect to relay");
    s.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
    s.set_nodelay(true).unwrap();
    let head = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).expect("send headers");
    s.flush().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(100));
    let sent_body = s.write_all(body.as_bytes()).and_then(|_| s.flush());
    let mut out = Vec::new();
    let read = s.read_to_end(&mut out);
    let text = String::from_utf8_lossy(&out).into_owned();
    match (sent_body, read) {
        (_, Ok(_)) if !text.is_empty() => text,
        (Err(e), _) => format!("<could not send the body: {e}; got so far: {text:?}>"),
        (_, Err(e)) => format!("<read failed: {e} ({:?}); got so far: {text:?}>", e.kind()),
        (_, Ok(_)) => "<connection closed with no reply>".into(),
    }
}

#[test]
fn relay_passes_through_unchanged_and_records_calls() {
    let _g = RELAY_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let up = fake_server();
    let db = Db::open_in_memory().unwrap();
    let tokio_rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let cfg = RelayConfig { enabled: true, port: 0, upstream: format!("http://127.0.0.1:{up}/v1") };
    let port = tokio_rt.block_on(relay::start(db.clone(), cfg)).unwrap();
    assert_eq!(relay::running_port(), Some(port));
    let base = format!("http://127.0.0.1:{port}");

    // Model list: forwarded, not recorded.
    let models: Value = ureq::get(&format!("{base}/v1/models")).call().unwrap().into_json().unwrap();
    assert_eq!(models["data"][0]["id"], "qwen3-8b");

    // Plain reply: body comes back byte-for-byte; reasoning, tool call and usage recorded.
    let direct = ureq::post(&format!("http://127.0.0.1:{up}/v1/chat/completions")).send_string(r#"{"model":"qwen3-8b","messages":[]}"#).unwrap().into_string().unwrap();
    let req = json!({"model": "qwen3-8b", "messages": [{"role": "system", "content": "Be brief."}, {"role": "user", "content": "Capital of France? my api_key=sk-abcdefghijklmnopqrstuvwxyz123456"}]});
    let via = ureq::post(&format!("{base}/v1/chat/completions")).set("User-Agent", "TestChat/1.0").send_json(req.clone()).unwrap().into_string().unwrap();
    assert_eq!(via, direct, "reply passed through unchanged");

    // Streamed reply with <think> tags, same conversation (same first system + user message), second turn.
    let mut req2 = req.clone();
    req2["stream"] = json!(true);
    req2["messages"].as_array_mut().unwrap().push(json!({"role": "assistant", "content": "Paris."}));
    req2["messages"].as_array_mut().unwrap().push(json!({"role": "user", "content": "Now a haiku."}));
    let resp = ureq::post(&format!("{base}/v1/chat/completions")).send_json(req2).unwrap();
    assert_eq!(resp.header("content-type"), Some("text/event-stream"));
    let sse = resp.into_string().unwrap();
    assert!(sse.contains("<think>The user") && sse.ends_with("data: [DONE]\n\n"), "stream passed through unchanged: {sse}");

    let r = wait_rows(&db, 2);
    assert_eq!(r.len(), 2, "two chat calls recorded, model list not recorded");
    let a = &r[0];
    assert_eq!(a["api"], "chat_completions");
    assert_eq!(a["model"], "qwen3-8b");
    assert_eq!(a["client"], "TestChat/1.0");
    assert_eq!(a["reply_redacted"], "Paris.");
    assert_eq!(a["reasoning_redacted"], "France's capital is Paris.");
    assert_eq!(a["reasoning_source"], "reasoning_content");
    assert_eq!(a["tool_calls"][0]["name"], "lookup");
    assert_eq!(a["finish_reason"], "tool_calls");
    assert_eq!(a["usage"]["prompt_tokens"], 20);
    assert_eq!(a["streamed"], 0);
    assert_eq!(a["http_status"], 200);
    assert!(!a["last_user_redacted"].as_str().unwrap().contains("sk-abcdefghijklmnopqrstuvwxyz123456"), "secret redacted: {}", a["last_user_redacted"]);
    assert!(!a["request_redacted"].to_string().contains("sk-abcdefghijklmnopqrstuvwxyz123456"));
    let b = &r[1];
    assert_eq!(b["conversation_id"], a["conversation_id"], "same conversation");
    assert_eq!(b["streamed"], 1);
    assert_eq!(b["reply_redacted"], "Autumn leaves fall");
    assert_eq!(b["reasoning_redacted"], "The user wants a haiku.");
    assert_eq!(b["reasoning_source"], "think_tags");
    assert_eq!(b["last_user_redacted"], "Now a haiku.");
    assert_eq!(b["message_count"], 4);
    assert_eq!(b["usage"]["completion_tokens"], 9);
    // Stream details: finished properly, chat app stayed connected, timing recorded.
    assert_eq!(b["end_marker"], 1);
    assert_eq!(b["downstream_closed"], 0);
    assert!(b["chunk_count"].as_i64().unwrap() >= 1);
    assert!(b["first_chunk_at"].as_i64().unwrap() >= b["started_at"].as_i64().unwrap());
    assert!(b["last_chunk_at"].as_i64().unwrap() <= b["finished_at"].as_i64().unwrap());
    assert!(b["error"].is_null());
    let mut b2 = b.clone();
    relay::annotate_call(&mut b2);
    assert_eq!(b2["outcome"]["state"], "complete");

    // The chain holds only hashes, so deleting a recording leaves no chat text behind.
    let chain: String = db.conn().query_row("SELECT group_concat(event_canonical) FROM chain_entries", [], |x| x.get(0)).unwrap();
    assert!(chain.contains(a["request_sha256"].as_str().unwrap()));
    assert!(!chain.contains("Paris") && !chain.contains("haiku"));

    // Host check, sent as raw HTTP/1.1 so exactly one Host header goes out and nothing depends on
    // how an HTTP client library treats an overridden Host. The body follows the headers a moment
    // later, the way real clients often send it, so the refusal must not leave it unread.
    let body = req.to_string();
    let refused = raw_post(port, "evil.example", &body);
    assert!(refused.starts_with("HTTP/1.1 403"), "a request addressed to another host name must get 403; got: {refused:?}");
    assert!(refused.contains("only accepts requests addressed to 127.0.0.1 or localhost"), "got: {refused:?}");
    for ok_host in [format!("localhost:{port}"), format!("127.0.0.1:{port}")] {
        let accepted = raw_post(port, &ok_host, &body);
        assert!(accepted.starts_with("HTTP/1.1 200"), "Host {ok_host} must be accepted; got: {accepted:?}");
    }
    assert_eq!(wait_rows(&db, 4).len(), 4, "the two accepted raw calls were recorded, the refused one was not");

    relay::stop();
    assert_eq!(relay::running_port(), None);
}

#[test]
fn unreachable_server_is_reported_to_the_chat_app_and_recorded() {
    let _g = RELAY_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    // Bind and drop to get a port nothing listens on.
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let db = Db::open_in_memory().unwrap();
    let tokio_rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let port = tokio_rt.block_on(relay::start(db.clone(), RelayConfig { enabled: true, port: 0, upstream: format!("http://127.0.0.1:{dead}") })).unwrap();
    let r = ureq::post(&format!("http://127.0.0.1:{port}/v1/chat/completions")).send_json(json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]}));
    match r {
        Err(ureq::Error::Status(502, resp)) => assert!(resp.into_string().unwrap().contains("could not reach your model server")),
        other => panic!("expected 502, got {other:?}"),
    }
    let rows = wait_rows(&db, 1);
    assert_eq!(rows.len(), 1);
    assert!(rows[0]["error"].as_str().unwrap().contains("could not reach"));
    assert!(rows[0]["http_status"].is_null());
    relay::stop();
}

#[test]
fn settings_are_checked() {
    let ok = RelayConfig { enabled: true, port: 1235, upstream: "http://127.0.0.1:1234/v1".into() };
    assert!(relay::validate(&ok).is_ok());
    assert!(relay::validate(&RelayConfig { upstream: "http://192.168.1.5:1234".into(), ..ok.clone() }).is_err(), "remote server refused");
    assert!(relay::validate(&RelayConfig { port: 1234, ..ok.clone() }).is_err(), "same port as the server refused");
    assert!(relay::validate(&RelayConfig { port: 80, ..ok.clone() }).is_err());
    assert_eq!(relay::upstream_root("http://127.0.0.1:1234/v1/"), "http://127.0.0.1:1234");
    // Addresses people copy from LM Studio / Ollama are accepted and pointed at this computer.
    assert_eq!(relay::upstream_root("localhost:1234"), "http://localhost:1234");
    assert_eq!(relay::upstream_root(" http://0.0.0.0:1234/v1 "), "http://127.0.0.1:1234");
    assert_eq!(relay::upstream_root("http://[::]:11434"), "http://127.0.0.1:11434");
    assert_eq!(relay::upstream_root("http://0.0.0.0.evil.example:1"), "http://0.0.0.0.evil.example:1");
    for up in ["localhost:1234", "http://0.0.0.0:1234", "http://LOCALHOST:1234/v1/", "http://127.0.0.2:1234", "http://[::1]:1234"] {
        let c = RelayConfig { upstream: relay::upstream_root(up), ..ok.clone() };
        assert!(relay::validate(&c).is_ok(), "{up} should be accepted");
    }
    assert!(relay::validate(&RelayConfig { upstream: "https://127.0.0.1:1234".into(), ..ok.clone() }).is_err());
    assert!(relay::validate(&RelayConfig { upstream: "http://lmstudio.example:1234".into(), ..ok.clone() }).is_err());
    assert!(relay::is_this_computer("localhost.") && relay::is_this_computer("[::1]") && !relay::is_this_computer("8.8.8.8"));
}

#[test]
fn parses_ollama_responses_api_and_bare_think_close() {
    let nd = format!("{}\n{}\n{}\n",
        json!({"message": {"role": "assistant", "content": "", "thinking": "Hmm, "}, "done": false}),
        json!({"message": {"role": "assistant", "content": "Hello", "thinking": "greet."}, "done": false}),
        json!({"message": {"role": "assistant", "content": "!"}, "done": true, "done_reason": "stop", "prompt_eval_count": 7, "eval_count": 3}));
    let p = parse_response("ollama_chat", Some("application/x-ndjson"), nd.as_bytes());
    assert!(p.streamed);
    assert_eq!(p.reply, "Hello!");
    assert_eq!(p.reasoning, "Hmm, greet.");
    assert_eq!(p.reasoning_source.as_deref(), Some("thinking"));
    assert_eq!(p.usage["completion_tokens"], 3);

    let resp = json!({"status": "completed", "output": [
        {"type": "reasoning", "summary": [{"type": "summary_text", "text": "Thinking briefly."}]},
        {"type": "function_call", "call_id": "c1", "name": "search", "arguments": "{}"},
        {"type": "message", "content": [{"type": "output_text", "text": "Done."}]}], "usage": {"input_tokens": 4, "output_tokens": 2}});
    let p = parse_response("responses", Some("application/json"), resp.to_string().as_bytes());
    assert_eq!(p.reply, "Done.");
    assert_eq!(p.reasoning, "Thinking briefly.");
    assert_eq!(p.tool_calls[0].1, "search");

    // Template already opened <think>, so only the closing tag appears in the reply.
    let p = parse_response("chat_completions", Some("application/json"), json!({"choices": [{"message": {"content": "plan it\n</think>\n\nAnswer"}}]}).to_string().as_bytes());
    assert_eq!(p.reply, "Answer");
    assert_eq!(p.reasoning, "plan it");
    assert_eq!(p.reasoning_source.as_deref(), Some("think_tags"));

    // Streamed tool-call arguments arrive in pieces and are joined by index.
    let sse = [
        json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "t1", "function": {"name": "read_file", "arguments": "{\"pa"}}]}}]}),
        json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "th\":\"a.txt\"}"}}]}}]}),
    ].iter().map(|v| format!("data: {v}\n\n")).collect::<String>();
    let p = parse_response("chat_completions", Some("text/event-stream"), sse.as_bytes());
    assert_eq!(p.tool_calls, vec![(Some("t1".into()), "read_file".into(), "{\"path\":\"a.txt\"}".into())]);
}


/// A saved `relay_calls` row as the conversation view reads it back.
fn saved_call(messages: Value, reply: &str, tool_calls: Value) -> Value {
    json!({"api": "chat_completions", "request_redacted": {"model": "qwen/qwen3.8-27b", "messages": messages},
           "reply_redacted": if reply.is_empty() { Value::Null } else { json!(reply) }, "tool_calls": tool_calls, "last_user_redacted": null})
}

#[test]
fn typed_tool_call_in_a_json_fence_is_flagged() {
    let typed = "I'll look that up.\n```json\n{\"name\": \"search\", \"arguments\": {\"query\": \"crime rate\"}}\n```";
    assert!(relay::looks_like_typed_tool_call(typed));
    // In the history: an earlier assistant turn with no tool_calls and no tool result after it.
    let mut c = saved_call(json!([
        {"role": "system", "content": "You can use tools."},
        {"role": "user", "content": "Find the crime rate"},
        {"role": "assistant", "content": typed},
        {"role": "user", "content": "Well?"}]), "Sorry, here it is.", Value::Null);
    relay::annotate_call(&mut c);
    assert_eq!(c["typed_tool_calls_in_history"], 1);
    assert_eq!(c["typed_tool_call_in_reply"], false);
    assert_eq!(c["typed_tool_call"], true);
    assert_eq!(c["latest_user"], "Well?");
    assert_eq!(c["earlier_messages"].as_array().unwrap().len(), 3);
    assert_eq!(c["earlier_messages"][2]["role"], "assistant");
    // In the reply.
    let mut c = saved_call(json!([{"role": "user", "content": "Find the crime rate"}]), typed, Value::Null);
    relay::annotate_call(&mut c);
    assert_eq!(c["typed_tool_call_in_reply"], true);
    assert_eq!(c["typed_tool_call"], true);
    assert_eq!(c["earlier_messages"].as_array().unwrap().len(), 0);
}

#[test]
fn bare_typed_tool_call_is_flagged() {
    let bare = r#"{"name": "read_file", "arguments": {"path": "notes.txt"}}"#;
    assert!(relay::looks_like_typed_tool_call(bare));
    assert!(relay::looks_like_typed_tool_call(&format!("Calling it now: {bare} (waiting)")));
    assert!(relay::looks_like_typed_tool_call(r#"[{"name": "search", "arguments": "{\"q\": 1}"}]"#));
    let mut c = saved_call(json!([{"role": "user", "content": "Read notes.txt"}]), bare, Value::Null);
    relay::annotate_call(&mut c);
    assert_eq!(c["typed_tool_call_in_reply"], true);
}

#[test]
fn real_tool_calls_are_not_flagged() {
    let args = r#"{"name": "search", "arguments": {"query": "x"}}"#;
    // The reply made a real tool call (the reply text happens to echo it).
    let mut c = saved_call(json!([{"role": "user", "content": "Search for x"}]), args,
        json!([{"id": "t1", "name": "search", "arguments": "{\"query\":\"x\"}"}]));
    relay::annotate_call(&mut c);
    assert_eq!(c["typed_tool_call"], false);
    // Earlier turns: one with a tool_calls entry, one followed by a tool-role result.
    let mut c = saved_call(json!([
        {"role": "user", "content": "Search for x"},
        {"role": "assistant", "content": args, "tool_calls": [{"id": "t1", "type": "function", "function": {"name": "search", "arguments": "{}"}}]},
        {"role": "tool", "tool_call_id": "t1", "content": "result"},
        {"role": "assistant", "content": args},
        {"role": "tool", "tool_call_id": "t2", "content": "result 2"},
        {"role": "user", "content": "Thanks"}]), "You're welcome.", Value::Null);
    relay::annotate_call(&mut c);
    assert_eq!(c["typed_tool_calls_in_history"], 0);
    assert_eq!(c["typed_tool_call"], false);
}

#[test]
fn normal_text_is_not_flagged() {
    for t in ["Hello! How can I help?", "Here is some JSON: {\"name\": \"Greg\", \"age\": 40}", "{\"arguments\": 1}", "```json\n{\"name\": \"x\"}\n```", "The word \"name\" and \"arguments\" but no JSON {here"] {
        assert!(!relay::looks_like_typed_tool_call(t), "flagged: {t}");
    }
    let mut c = saved_call(json!([
        {"role": "system", "content": "Be nice."},
        {"role": "user", "content": "Hi"},
        {"role": "assistant", "content": "Hello!"},
        {"role": "user", "content": "How are you?"}]), "Fine, thanks.", Value::Null);
    relay::annotate_call(&mut c);
    assert_eq!(c["typed_tool_call"], false);
    assert_eq!(c["latest_user"], "How are you?");
    assert_eq!(c["earlier_messages"].as_array().unwrap().len(), 3);
}

/// Saves one chat call the way the relay would.
fn record_call(db: &Db, messages: Value, reply: &str) {
    let req = json!({"model": "qwen/qwen3.8-27b", "messages": messages}).to_string();
    let resp = json!({"choices": [{"message": {"role": "assistant", "content": reply}, "finish_reason": "stop"}]}).to_string();
    relay::record(db, &relay::Call { api: "chat_completions", path: "/v1/chat/completions", request: req.as_bytes(), response: resp.as_bytes(),
        content_type: Some("application/json".into()), status: Some(200), started: 1, ..Default::default() }).unwrap();
}

#[test]
fn conversation_list_counts_typed_tool_requests() {
    let db = Db::open_in_memory().unwrap();
    let typed = "```json\n{\"name\": \"search\", \"arguments\": {\"query\": \"crime rate\"}}\n```";
    // Affected chat: the first reply is typed (flagged in the reply), and the second call re-sends
    // it in the history (flagged there), so 2 calls.
    let sys = json!({"role": "system", "content": "You can use tools."});
    let ask = json!({"role": "user", "content": "Find the crime rate"});
    record_call(&db, json!([sys, ask]), typed);
    record_call(&db, json!([sys, ask, {"role": "assistant", "content": typed}, {"role": "user", "content": "Well?"}]), "Sorry about that.");
    // Normal chat, including JSON that has a name but no arguments.
    record_call(&db, json!([{"role": "user", "content": "Hi"}]), "Hello! Here is JSON: {\"name\": \"Greg\"}");

    let list = relay::list_conversations(&db).unwrap();
    assert_eq!(list.len(), 2, "got: {list:?}");
    let by_title = |t: &str| list.iter().find(|c| c["title"] == t).unwrap_or_else(|| panic!("no {t} in {list:?}")).clone();
    let affected = by_title("Find the crime rate");
    assert_eq!(affected["calls"], 2);
    assert_eq!(affected["typed_tool_calls"], 2);
    assert_eq!(by_title("Hi")["typed_tool_calls"], 0);
    assert_eq!(affected["problems"], 0);
    assert_eq!(affected["interrupted"], 0);
}

// ---- how a reply ended, tools, timing, shortened text -------------------------------------------

fn sse(events: &[Value], done: bool) -> String {
    let mut out: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
    if done { out.push_str("data: [DONE]\n\n"); }
    out
}

fn delta(text: &str) -> Value { json!({"choices": [{"delta": {"reasoning_content": text}}]}) }

/// Records one streamed call and returns the saved row, annotated like the conversation view.
fn record_stream(body: &str, downstream_closed: bool) -> Value {
    let db = Db::open_in_memory().unwrap();
    let req = json!({"model": "qwen/qwen3.8-27b", "stream": true, "messages": [{"role": "user", "content": "what is a prime number thats a million numbers?"}]}).to_string();
    relay::record(&db, &relay::Call { api: "chat_completions", path: "/v1/chat/completions", request: req.as_bytes(), response: body.as_bytes(),
        content_type: Some("text/event-stream".into()), status: Some(200), started: 1_000, finished: 2_810_000, first_chunk_at: Some(3_000), last_chunk_at: Some(60_000),
        chunk_count: Some(42), bytes_received: Some(body.len() as i64), downstream_closed, ..Default::default() }).unwrap();
    let mut row = rows(&db).remove(0);
    relay::annotate_call(&mut row);
    row
}

#[test]
fn stream_without_end_marker_and_no_disconnect_has_unknown_cause() {
    let row = record_stream(&sse(&[delta("Let me think about primes"), delta(" with a million digits...")], false), false);
    assert_eq!(row["http_status"], 200);
    assert!(row["error"].is_null(), "an unfinished reply is not saved as an error: {}", row["error"]);
    assert_eq!(row["end_marker"], 0);
    assert_eq!(row["outcome"]["state"], "interrupted");
    assert_eq!(row["outcome"]["label"], "Interrupted");
    assert_eq!(row["outcome"]["cause"], "Stream ended before completion; cause unknown.");
    assert_eq!(row["outcome"]["by_chat_app"], false);
    assert_eq!(row["reasoning_redacted"], "Let me think about primes with a million digits...");
}

#[test]
fn stream_cut_by_the_chat_app_is_attributed_to_it() {
    let row = record_stream(&sse(&[delta("Thinking")], false), true);
    assert_eq!(row["downstream_closed"], 1);
    assert_eq!(row["outcome"]["state"], "interrupted");
    assert_eq!(row["outcome"]["by_chat_app"], true);
    assert!(row["outcome"]["cause"].as_str().unwrap().contains("chat app closed its connection"), "{}", row["outcome"]);
}

#[test]
fn completed_stream_needs_the_end_marker() {
    // finish_reason and [DONE]
    let done = record_stream(&sse(&[delta("ok"), json!({"choices": [{"delta": {"content": "Yes."}, "finish_reason": "stop"}]})], true), false);
    assert_eq!(done["end_marker"], 1);
    assert_eq!(done["outcome"]["state"], "complete");
    // [DONE] alone is enough too.
    assert_eq!(record_stream(&sse(&[delta("ok")], true), false)["outcome"]["state"], "complete");
    // A plain (not streamed) HTTP 200 reply without finish_reason is not counted as finished.
    let db = Db::open_in_memory().unwrap();
    let req = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]}).to_string();
    let resp = json!({"choices": [{"message": {"role": "assistant", "content": "Hel"}}]}).to_string();
    relay::record(&db, &relay::Call { api: "chat_completions", path: "/v1/chat/completions", request: req.as_bytes(), response: resp.as_bytes(),
        content_type: Some("application/json".into()), status: Some(200), started: 1, ..Default::default() }).unwrap();
    let mut row = rows(&db).remove(0);
    relay::annotate_call(&mut row);
    assert_eq!(row["outcome"]["state"], "interrupted");
    // Ollama: done:true is the marker.
    let p = parse_response("ollama_chat", Some("application/x-ndjson"), b"{\"message\":{\"content\":\"a\"},\"done\":false}\n{\"message\":{\"content\":\"b\"},\"done\":true}\n");
    assert!(p.end_marker);
    let p = parse_response("ollama_chat", Some("application/x-ndjson"), b"{\"message\":{\"content\":\"a\"},\"done\":false}\n");
    assert!(!p.end_marker);
}

#[test]
fn real_errors_stay_problems_and_older_recordings_fall_back() {
    let outcome = |row: Value| relay::call_outcome(&row);
    assert_eq!(outcome(json!({"http_status": null, "error": "Houndbit could not reach your model server"}))["state"], "problem");
    assert_eq!(outcome(json!({"http_status": 500, "error": "boom"}))["state"], "problem");
    assert_eq!(outcome(json!({"http_status": 200, "error": "Model not loaded"}))["state"], "problem");
    // Older recordings (no end_marker / downstream_closed columns filled in).
    let old = outcome(json!({"http_status": 200, "error": null, "finish_reason": null}));
    assert_eq!(old["state"], "interrupted");
    assert_eq!(old["cause"], relay::CAUSE_UNKNOWN);
    let old_cut = outcome(json!({"http_status": 200, "error": "The model server's reply was cut off: timed out reading response", "finish_reason": null}));
    assert_eq!(old_cut["state"], "interrupted");
    assert_eq!(old_cut["cause"], relay::CAUSE_UNKNOWN);
    assert!(old_cut["detail"].as_str().unwrap().contains("timed out"));
    // That older note was only saved when sending to the chat app failed, so it counts as a seen disconnect.
    let old_app = outcome(json!({"http_status": 200, "error": "The chat app stopped the reply early (Stop button, or the app closed the connection).", "finish_reason": null}));
    assert_eq!(old_app["by_chat_app"], true);
    assert_eq!(outcome(json!({"http_status": 200, "error": null, "finish_reason": "stop"}))["state"], "complete");
}

#[test]
fn tools_are_split_into_available_requested_typed_and_run() {
    let typed = "```json\n{\"name\": \"search_knowledge_bases\", \"arguments\": {\"query\": \"paraguay population\"}}\n```";
    let tools = json!([{"type": "function", "function": {"name": "search_knowledge_bases"}}, {"type": "function", "function": {"name": "write_note"}}]);
    // The user's case: typed-as-text requests in the history, no tool_calls, no tool results.
    let mut c = saved_call(json!([
        {"role": "user", "content": "tell me what is the population of paragua?"},
        {"role": "assistant", "content": typed},
        {"role": "user", "content": "create a file named testaj102.txt"},
        {"role": "assistant", "content": "```json\n{\"name\": \"write_note\", \"arguments\": {\"title\": \"testaj102.txt\"}}\n```"},
        {"role": "user", "content": "think about this"}]), "", Value::Null);
    c["request_redacted"]["tools"] = tools.clone();
    relay::annotate_call(&mut c);
    assert_eq!(c["tools_available"], json!(["search_knowledge_bases", "write_note"]));
    assert_eq!(c["tools_requested"], json!([]));
    let typed_names: Vec<&str> = c["tools_typed_as_text"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(typed_names, ["search_knowledge_bases", "write_note"]);
    assert_eq!(c["tools_executed"], json!([]), "typed-as-text requests are never counted as run");

    // Real structured calls: one with a matching tool result (run), one without (not run),
    // plus a tool result whose id matches nothing (not evidence).
    let mut c = saved_call(json!([
        {"role": "user", "content": "Look up x and y"},
        {"role": "assistant", "content": null, "tool_calls": [
            {"id": "call_a", "type": "function", "function": {"name": "search_knowledge_bases", "arguments": "{}"}},
            {"id": "call_b", "type": "function", "function": {"name": "write_note", "arguments": "{}"}}]},
        {"role": "tool", "tool_call_id": "call_a", "content": "found it"},
        {"role": "tool", "tool_call_id": "call_zzz", "content": "stray"},
        {"role": "user", "content": "thanks"}]), "", json!([{"id": "call_c", "name": "write_note", "arguments": "{}"}]));
    c["request_redacted"]["tools"] = tools;
    relay::annotate_call(&mut c);
    let req = c["tools_requested"].as_array().unwrap();
    assert_eq!(req.len(), 3);
    assert_eq!(req[2]["where"], "this reply");
    let run = c["tools_executed"].as_array().unwrap();
    assert_eq!(run.len(), 1);
    assert_eq!(run[0]["id"], "call_a");
    assert_eq!(run[0]["name"], "search_knowledge_bases");
    assert_eq!(c["tool_results_unmatched"], 1);
    assert_eq!(c["tools_typed_as_text"], json!([]));

    // Across a conversation: the reply's call_c counts as run once a later request sends its result.
    let later = saved_call(json!([
        {"role": "user", "content": "Look up x and y"},
        {"role": "assistant", "content": null, "tool_calls": [{"id": "call_c", "type": "function", "function": {"name": "write_note", "arguments": "{}"}}]},
        {"role": "tool", "tool_call_id": "call_c", "content": "saved"}]), "Done.", Value::Null);
    let mut calls = vec![c, later];
    relay::annotate_conversation(&mut calls);
    assert!(calls[0]["tools_executed"].as_array().unwrap().iter().any(|t| t["id"] == "call_c" && t["where"] == "result sent with a later message"));

    // Responses API: function_call + function_call_output with the same call_id.
    let mut r = json!({"api": "responses", "request_redacted": {"input": [
        {"role": "user", "content": "go"},
        {"type": "function_call", "call_id": "fc_1", "name": "lookup", "arguments": "{}"},
        {"type": "function_call_output", "call_id": "fc_1", "output": "ok"}]}, "reply_redacted": "fine", "tool_calls": null});
    relay::annotate_call(&mut r);
    assert_eq!(r["tools_executed"][0]["name"], "lookup");
}

#[test]
fn shortened_text_is_flagged() {
    let db = Db::open_in_memory().unwrap();
    let long_thinking = "a".repeat(100_050);
    let long_message = "b".repeat(20_010);
    let req = json!({"model": "m", "stream": true, "messages": [{"role": "user", "content": long_message}]}).to_string();
    let body = sse(&[delta(&long_thinking), json!({"choices": [{"delta": {"content": "short reply"}, "finish_reason": "stop"}]})], true);
    relay::record(&db, &relay::Call { api: "chat_completions", path: "/v1/chat/completions", request: req.as_bytes(), response: body.as_bytes(),
        content_type: Some("text/event-stream".into()), status: Some(200), started: 1, bytes_received: Some(body.len() as i64), ..Default::default() }).unwrap();
    let mut row = rows(&db).remove(0);
    relay::annotate_call(&mut row);
    let parts = row["shortened"].as_array().unwrap();
    assert!(parts.contains(&json!({"part": "thinking", "limit": 100_000, "unit": "characters"})), "{parts:?}");
    assert!(parts.contains(&json!({"part": "full request", "limit": 20_000, "unit": "characters"})), "{parts:?}");
    assert!(!parts.iter().any(|p| p["part"] == "reply"));
    assert!(row["reasoning_redacted"].as_str().unwrap().ends_with("[... 50 more characters not kept]"));
    // Nothing shortened: nothing flagged, even if the text mentions the words.
    let plain = record_stream(&sse(&[json!({"choices": [{"delta": {"content": "I kept [... a few more characters not kept] of it"}, "finish_reason": "stop"}]})], true), false);
    assert_eq!(plain["shortened"], json!([]));
    // Bigger than the recorder keeps (bytes_received > bytes kept): flagged, and the end marker is
    // still found in the tail.
    let db = Db::open_in_memory().unwrap();
    let head = sse(&[delta("start")], false);
    let tail = format!("partial line\n{}", sse(&[json!({"choices": [{"delta": {}, "finish_reason": "stop"}]})], true));
    relay::record(&db, &relay::Call { api: "chat_completions", path: "/v1/chat/completions", request: req.as_bytes(), response: head.as_bytes(), tail: tail.as_bytes(),
        content_type: Some("text/event-stream".into()), status: Some(200), started: 1, bytes_received: Some(20 << 20), ..Default::default() }).unwrap();
    let mut row = rows(&db).remove(0);
    relay::annotate_call(&mut row);
    assert_eq!(row["capture_cut"], 1);
    assert_eq!(row["outcome"]["state"], "complete");
    assert!(row["shortened"].as_array().unwrap().iter().any(|p| p["unit"] == "MB" && p["limit"] == 16));
}

/// A model server that streams a few pieces with pauses, stays quiet, then closes without the
/// end-of-reply marker. `pieces` events, `gap_ms` between them, `quiet_ms` before closing.
fn cut_server(pieces: usize, gap_ms: u64, quiet_ms: u64) -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { continue };
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut len = 0usize;
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() { break; }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") { len = v.trim().parse().unwrap(); }
            }
            let mut body = vec![0; len];
            let _ = r.read_exact(&mut body);
            let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n");
            for i in 0..pieces {
                if write!(s, "data: {}\n\n", delta(&format!("step {i} "))).and_then(|_| s.flush()).is_err() { break; }
                std::thread::sleep(std::time::Duration::from_millis(gap_ms));
            }
            std::thread::sleep(std::time::Duration::from_millis(quiet_ms));
        }
    });
    port
}

#[test]
fn relay_records_timing_and_an_unfinished_stream() {
    let _g = RELAY_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let up = cut_server(3, 200, 1_200);
    let db = Db::open_in_memory().unwrap();
    let tokio_rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let port = tokio_rt.block_on(relay::start(db.clone(), RelayConfig { enabled: true, port: 0, upstream: format!("http://127.0.0.1:{up}") })).unwrap();
    let got = ureq::post(&format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .send_json(json!({"model": "m", "stream": true, "messages": [{"role": "user", "content": "primes"}]})).unwrap().into_string().unwrap();
    assert!(got.contains("step 2"), "the chat app got the whole stream: {got}");
    let r = wait_rows(&db, 1);
    assert_eq!(r.len(), 1);
    let mut row = r[0].clone();
    let (start, first, last, fin) = (row["started_at"].as_i64().unwrap(), row["first_chunk_at"].as_i64().unwrap(), row["last_chunk_at"].as_i64().unwrap(), row["finished_at"].as_i64().unwrap());
    assert!(start <= first && first <= last && last <= fin);
    assert!(last - first >= 300, "pieces arrived over about 400 ms: first {first}, last {last}");
    assert!(fin - last >= 1_000, "about 1.2 s of quiet before the close is measured: last {last}, closed {fin}");
    assert!(row["chunk_count"].as_i64().unwrap() >= 2);
    assert_eq!(row["downstream_closed"], 0);
    assert_eq!(row["end_marker"], 0);
    relay::annotate_call(&mut row);
    assert_eq!(row["outcome"]["cause"], relay::CAUSE_UNKNOWN);
    relay::stop();
}

#[test]
fn relay_notices_when_the_chat_app_hangs_up() {
    let _g = RELAY_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let up = cut_server(40, 100, 0);
    let db = Db::open_in_memory().unwrap();
    let tokio_rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let port = tokio_rt.block_on(relay::start(db.clone(), RelayConfig { enabled: true, port: 0, upstream: format!("http://127.0.0.1:{up}") })).unwrap();
    {
        let body = json!({"model": "m", "stream": true, "messages": [{"role": "user", "content": "primes"}]}).to_string();
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(s, "POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        let mut first = [0u8; 256];
        let _ = s.read(&mut first);
        // Hang up after the first bit, like pressing Stop.
    }
    let mut row = Value::Null;
    for _ in 0..100 {
        if let Some(r) = rows(&db).into_iter().next() { row = r; break; }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(!row.is_null(), "call recorded");
    assert_eq!(row["downstream_closed"], 1, "{row}");
    assert!(row["chunk_count"].as_i64().unwrap() < 40, "the recorder stopped reading once the chat app was gone");
    relay::annotate_call(&mut row);
    assert_eq!(row["outcome"]["by_chat_app"], true);
    relay::stop();
}

#[test]
fn context_full_error_from_lm_studio_is_explained_with_request_size() {
    let db = Db::open_in_memory().unwrap();
    let tool = json!({"type": "function", "function": {"name": "search_knowledge_bases", "description": "Search the user's accessible knowledge bases by name and description.", "parameters": {"type": "object", "properties": {"query": {"type": "string"}}}}});
    let req = json!({"model": "qwen/qwen3.8-27b", "stream": true, "messages": [{"role": "user", "content": "what is the first prime number after 100?"}], "tools": vec![tool; 38]}).to_string();
    let resp = "data: {\"error\":{\"message\":\"Engine protocol predict stream returned an error: {\\\"code\\\":500,\\\"message\\\":\\\"Context size has been exceeded.\\\",\\\"type\\\":\\\"server_error\\\"}\"}}\n\n";
    relay::record(&db, &relay::Call { api: "chat_completions", path: "/v1/chat/completions", request: req.as_bytes(), response: resp.as_bytes(),
        content_type: Some("text/event-stream".into()), status: Some(200), started: 1, first_chunk_at: Some(89_801), last_chunk_at: Some(89_802), chunk_count: Some(2), ..Default::default() }).unwrap();
    let mut row = rows(&db).remove(0);
    relay::annotate_call(&mut row);
    let o = &row["outcome"];
    assert_eq!(o["state"], "problem");
    assert_eq!(o["label"], "Model server error");
    assert_eq!(o["source"], "model_server");
    assert_eq!(o["kind"], "context_full");
    assert!(o["detail"].as_str().unwrap().contains("Context size has been exceeded"));
    let cause = o["cause"].as_str().unwrap();
    assert!(cause.contains("context window") && cause.contains("38 tool descriptions") && cause.contains("rough estimate"), "{cause}");
    let size = &row["request_size"];
    assert_eq!(size["tools_count"], 38);
    assert!(size["tools_chars"].as_u64().unwrap() > 20 * size["messages_chars"].as_u64().unwrap());
    assert!(size["est_tokens_low"].as_u64().unwrap() < size["est_tokens_high"].as_u64().unwrap());
    // Other server errors keep the server's words but don't guess a cause.
    let other = relay::call_outcome(&json!({"http_status": 200, "error": "Model not loaded"}));
    assert_eq!(other["label"], "Model server error");
    assert!(other["kind"].is_null());
    assert_eq!(relay::call_outcome(&json!({"http_status": null, "error": "could not connect"}))["label"], "No answer");
    assert!(!relay::is_context_full_error("Model not loaded"));
    assert!(relay::is_context_full_error("This model's maximum context length is 8192 tokens"));
}
