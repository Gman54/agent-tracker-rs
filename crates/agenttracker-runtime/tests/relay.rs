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
    relay::record(db, &relay::Call { api: "chat_completions", path: "/v1/chat/completions", client: None, request: req.as_bytes(), response: resp.as_bytes(),
        content_type: Some("application/json".into()), status: Some(200), error: None, started: 1 }).unwrap();
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
}
