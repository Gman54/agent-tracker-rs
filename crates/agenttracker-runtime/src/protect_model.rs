//! The local model helper: a model running on your own computer (LM Studio or Ollama) works on
//! one of Houndbit's Protect folders, and you decide what happens to your real files.
//!
//! The model never gets a shell, code execution or the network. Each step it may only ask for one
//! of Protect's staged worker operations (list, read, write a staged copy) or say it is done.
//! Houndbit submits those as normal Protect actions with its own key, so the policy, the deny
//! list and the records apply exactly as for any other app. Writes land in Protect's staging
//! folder. Nothing reaches your real folder until you look at each change and press Apply, which
//! goes through Protect's own controls (the admin path), never through anything the model can ask
//! for. Another model can't approve anything either: only your click applies a change.
use crate::protect;
use crate::provider::{ChatMessage, ChatRequest, Provider};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// How many replies the model gets before the helper stops it.
pub const MAX_STEPS: usize = 30;
/// Replies in a row that aren't a usable action before the helper gives up.
const MAX_BAD_REPLIES: usize = 3;
/// Jobs kept in memory for the page (oldest dropped first).
const KEEP_JOBS: usize = 20;
/// How long the helper waits for your OK when Protect's policy asks for one.
const APPROVAL_WAIT_SECS: u64 = 15 * 60;

#[derive(Clone, Serialize)]
pub struct Job {
    pub id: String,
    /// working, stopping, review (changes waiting for you), finished, failed
    pub state: String,
    pub grant_id: String,
    pub folder: String,
    pub request: String,
    pub model: String,
    pub endpoint: String,
    /// false when the model server is not on this computer; your file contents go to it.
    pub local: bool,
    pub session_id: Option<String>,
    /// What happened, in plain words, oldest first.
    pub steps: Vec<String>,
    pub summary: Option<String>,
    pub error: Option<String>,
    pub started_at: i64,
}

static JOBS: Mutex<Vec<Job>> = Mutex::new(Vec::new());

fn jobs() -> std::sync::MutexGuard<'static, Vec<Job>> {
    JOBS.lock().unwrap_or_else(|p| p.into_inner())
}

fn update(id: &str, f: impl FnOnce(&mut Job)) {
    if let Some(j) = jobs().iter_mut().find(|j| j.id == id) {
        f(j);
    }
}

fn log(id: &str, line: impl Into<String>) {
    let line = line.into();
    update(id, |j| j.steps.push(line));
}

fn state_of(id: &str) -> Option<String> {
    jobs().iter().find(|j| j.id == id).map(|j| j.state.clone())
}

/// True when the endpoint's host is this computer.
pub fn endpoint_is_local(endpoint: &str) -> bool {
    let ep = endpoint.trim();
    let rest = ep.strip_prefix("http://").or_else(|| ep.strip_prefix("https://")).unwrap_or(ep);
    let host_port = rest.split('/').next().unwrap_or("");
    let host = if host_port.starts_with('[') { host_port.split(']').next().unwrap_or("").trim_start_matches('[') } else { host_port.split(':').next().unwrap_or("") };
    matches!(host, "127.0.0.1" | "localhost" | "::1") || host.starts_with("127.")
}

/// The JSON every model reply must be. Strict-schema friendly: all fields required.
pub fn action_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "tool": {"type": "string", "enum": ["list", "read", "write", "done"]},
            "path": {"type": "string"},
            "text": {"type": "string"},
            "note": {"type": "string"}
        },
        "required": ["tool", "path", "text", "note"],
        "additionalProperties": false
    })
}

fn system_prompt() -> String {
    "You are helping with files in one folder on the user's computer. You work through Houndbit Protect, which only lets you do these things.\n\
Reply with exactly one JSON object and nothing else: {\"tool\": ..., \"path\": ..., \"text\": ..., \"note\": ...}\n\
- tool \"list\": see the files. path is \"\" for the whole folder or a sub-folder.\n\
- tool \"read\": read one file. path is the file.\n\
- tool \"write\": suggest new contents for one file. path is the file, text is the WHOLE new file (not a patch). New files are fine. This only makes a staged copy; the user reviews it before anything changes.\n\
- tool \"done\": you are finished. note is a short summary for the user of what you changed and why.\n\
Use \"\" for fields a tool doesn't need. Paths are relative to the folder and use forward slashes.\n\
You cannot run commands, use the internet, delete files, or read secrets such as .env or keys (Protect refuses those).\n\
Read a file before rewriting it. If a read says the file was cut short, do not rewrite that file.\n\
Keep going one step at a time until the request is done, then use \"done\"."
        .to_string()
}

/// Starts a job in the background and returns it. `provider` is the model to use (already
/// checked to be allowed); `endpoint` and `model` are just for showing.
pub fn start(home: PathBuf, provider: Arc<dyn Provider>, endpoint: &str, model: &str, grant_id: &str, folder: &str, request: &str, context_chars: usize) -> Result<Value, String> {
    if !protect::is_hosted() {
        return Err("The model helper needs Protect running inside Houndbit. A separate Protect program is answering right now.".into());
    }
    let request = request.trim();
    if request.is_empty() {
        return Err("Type what you'd like the model to do first.".into());
    }
    if request.chars().count() > 4000 {
        return Err("That request is very long. Keep it under 4,000 characters.".into());
    }
    if grant_id.trim().is_empty() {
        return Err("Pick one of Houndbit's folders first.".into());
    }
    {
        let js = jobs();
        if js.iter().any(|j| j.state == "working" || j.state == "stopping") {
            return Err("The helper is already working on something. Wait for it to finish or press Stop.".into());
        }
    }
    let job = Job {
        id: crate::ids::new_id("mjob"),
        state: "working".into(),
        grant_id: grant_id.to_string(),
        folder: folder.to_string(),
        request: request.to_string(),
        model: model.to_string(),
        endpoint: endpoint.to_string(),
        local: endpoint_is_local(endpoint),
        session_id: None,
        steps: vec![],
        summary: None,
        error: None,
        started_at: protect_engine::now_ms(),
    };
    let id = job.id.clone();
    {
        let mut js = jobs();
        js.push(job);
        let extra = js.len().saturating_sub(KEEP_JOBS);
        js.drain(0..extra);
    }
    let (gid, req) = (grant_id.to_string(), request.to_string());
    let jid = id.clone();
    tokio::spawn(async move { run(jid, home, provider, gid, req, context_chars).await });
    view(&id).ok_or_else(|| "The job disappeared right after starting.".into())
}

/// Asks a working job to stop after its current step. Staged changes so far stay for review.
pub fn stop(id: &str) -> Result<Value, String> {
    let mut found = false;
    update(id, |j| {
        found = true;
        if j.state == "working" {
            j.state = "stopping".into();
        }
    });
    if !found {
        return Err("That job isn't here any more.".into());
    }
    view(id).ok_or_else(|| "That job isn't here any more.".into())
}

/// The job, plus the changes waiting for review (with old and new text) when there are any.
pub fn view(id: &str) -> Option<Value> {
    let job = jobs().iter().find(|j| j.id == id).cloned()?;
    let mut v = serde_json::to_value(&job).ok()?;
    if let (Some(sid), true) = (&job.session_id, matches!(job.state.as_str(), "review" | "finished" | "failed")) {
        match protect::admin_changes(sid) {
            Ok(c) => v["changes"] = c["changes"].clone(),
            Err(e) => v["changes_error"] = json!(e),
        }
    }
    v["max_steps"] = json!(MAX_STEPS);
    Some(v)
}

/// The most recent job, if any.
pub fn latest() -> Option<Value> {
    let id = jobs().last().map(|j| j.id.clone())?;
    view(&id)
}

fn session_for_decision(id: &str) -> Result<String, String> {
    let job = jobs().iter().find(|j| j.id == id).cloned().ok_or("That job isn't here any more.")?;
    if job.state == "working" || job.state == "stopping" {
        return Err("Wait until the helper has finished before applying anything.".into());
    }
    job.session_id.ok_or_else(|| "This job never got as far as making changes.".into())
}

fn settle(id: &str, sid: &str) {
    let left = protect::admin_changes(sid).map(|c| c["changes"].as_array().map(|a| a.len()).unwrap_or(0)).unwrap_or(0);
    update(id, |j| {
        if left == 0 && j.state == "review" {
            j.state = "finished".into();
        }
    });
}

/// Writes the chosen changes (None = all) into your real folder. Only called from your click.
pub fn apply(id: &str, paths: Option<Vec<String>>) -> Result<Value, String> {
    let sid = session_for_decision(id)?;
    let r = protect::admin_apply(&sid, paths.as_deref())?;
    for x in r["results"].as_array().into_iter().flatten() {
        let p = x["path"].as_str().unwrap_or("?");
        if x["applied"] == true {
            log(id, format!("You applied the change to {p}. Your real file was written."));
        } else {
            log(id, format!("{p} was not applied: {}", x["reason"].as_str().unwrap_or("unknown reason")));
        }
    }
    settle(id, &sid);
    let mut v = view(id).unwrap_or(Value::Null);
    v["results"] = r["results"].clone();
    Ok(v)
}

/// Throws away the chosen changes (None = all). Your real folder isn't touched.
pub fn discard(id: &str, paths: Option<Vec<String>>) -> Result<Value, String> {
    let sid = session_for_decision(id)?;
    let r = protect::admin_discard(&sid, paths.as_deref())?;
    for x in r["results"].as_array().into_iter().flatten() {
        if x["discarded"] == true {
            log(id, format!("You threw away the change to {}.", x["path"].as_str().unwrap_or("?")));
        }
    }
    settle(id, &sid);
    Ok(view(id).unwrap_or(Value::Null))
}

async fn run(id: String, home: PathBuf, provider: Arc<dyn Provider>, grant_id: String, request: String, context_chars: usize) {
    let result = work(&id, &home, provider, &grant_id, &request, context_chars).await;
    let changes = jobs().iter().find(|j| j.id == id).and_then(|j| j.session_id.clone()).map(|sid| {
        protect::admin_changes(&sid).map(|c| c["changes"].as_array().map(|a| a.len()).unwrap_or(0)).unwrap_or(0)
    });
    let n = changes.unwrap_or(0);
    update(&id, |j| match result {
        Ok(summary) => {
            j.summary = Some(summary);
            j.state = if n > 0 { "review".into() } else { "finished".into() };
        }
        Err(e) => {
            j.error = Some(e);
            // Anything it staged before the problem can still be looked at and applied.
            j.state = if n > 0 { "review".into() } else { "failed".into() };
        }
    });
    if n > 0 {
        log(&id, format!("{n} change{} waiting for you to look over.", if n == 1 { " is" } else { "s are" }));
    }
}

/// Cuts text to `max` characters, saying so.
fn cut(s: &str, max: usize) -> (String, bool) {
    if s.chars().count() <= max {
        return (s.to_string(), false);
    }
    (s.chars().take(max).collect::<String>(), true)
}

/// Keeps the conversation inside the budget by blanking the oldest tool results first.
fn compact(messages: &mut [ChatMessage], budget: usize) {
    let total = |m: &[ChatMessage]| m.iter().map(|x| x.content.len()).sum::<usize>();
    let mut i = 2;
    while total(messages) > budget && i + 1 < messages.len() {
        if messages[i].role == "user" && messages[i].content.len() > 200 {
            messages[i].content = "(An older result was removed to save room. Read the file again if you need it.)".into();
        } else if messages[i].role == "assistant" && messages[i].content.len() > 400 {
            messages[i].content = "(An older reply of yours was shortened to save room.)".into();
        }
        i += 1;
    }
}

async fn work(id: &str, home: &Path, provider: Arc<dyn Provider>, grant_id: &str, request: &str, context_chars: usize) -> Result<String, String> {
    let mut c = protect::signed_in_client(home).await?;
    let s = c.call("v1.sessions.request", json!({"resource_grant_id": grant_id})).await.map_err(|e| e.message)?;
    let sid = s["id"].as_str().ok_or("Protect didn't return a session id")?.to_string();
    update(id, |j| j.session_id = Some(sid.clone()));
    c.call("v1.sessions.start", json!({"session_id": sid})).await.map_err(|e| e.message)?;
    log(id, format!("Protect made a private copy of the folder ({} files) for the model to work in.", s["snapshot_files"].as_i64().unwrap_or(0)));

    let budget = context_chars.max(8_000);
    let read_cap = (budget / 3).clamp(2_000, 40_000);
    let mut messages = vec![
        ChatMessage { role: "system".into(), content: system_prompt() },
        ChatMessage { role: "user".into(), content: format!("Request from the user:\n{request}\n\nStart by listing the files.") },
    ];
    let mut cut_files: HashSet<String> = HashSet::new();
    let mut bad = 0usize;
    let mut summary: Option<String> = None;
    let outcome: Result<(), String> = async {
        for step in 1..=MAX_STEPS {
            if state_of(id).as_deref() == Some("stopping") {
                log(id, "You pressed Stop, so the helper stopped.");
                summary = Some("Stopped before the model said it was done.".into());
                return Ok(());
            }
            compact(&mut messages, budget);
            let req = ChatRequest { messages: messages.clone(), schema: action_schema(), seed: None };
            let p = provider.clone();
            let reply = tokio::task::spawn_blocking(move || p.complete(&req)).await.map_err(|e| format!("The model call stopped unexpectedly: {e}"))?;
            let reply = reply.map_err(|e| format!("The model didn't answer: {e}"))?;
            let action = crate::proposal::extract_json(&reply.content).ok().map(|(v, _)| v).filter(|v| v["tool"].is_string());
            let Some(action) = action else {
                bad += 1;
                log(id, format!("Step {step}: the model's reply wasn't a usable action."));
                if bad >= MAX_BAD_REPLIES {
                    return Err(format!("The model didn't reply in the expected format {MAX_BAD_REPLIES} times in a row, so the helper stopped. A larger or instruction-following model usually does better."));
                }
                let (shown, _) = cut(&reply.content, 600);
                messages.push(ChatMessage { role: "assistant".into(), content: shown });
                messages.push(ChatMessage { role: "user".into(), content: "That wasn't a single JSON action. Reply with only {\"tool\": \"list|read|write|done\", \"path\": \"...\", \"text\": \"...\", \"note\": \"...\"}.".into() });
                continue;
            };
            bad = 0;
            let tool = action["tool"].as_str().unwrap_or("").to_string();
            let path = action["path"].as_str().unwrap_or("").trim().trim_start_matches("./").to_string();
            let text = action["text"].as_str().unwrap_or("").to_string();
            messages.push(ChatMessage { role: "assistant".into(), content: action.to_string() });

            let (op, args) = match tool.as_str() {
                "done" => {
                    let note = action["note"].as_str().unwrap_or("").trim().to_string();
                    log(id, "The model said it is done.");
                    summary = Some(if note.is_empty() { "The model finished without a summary.".into() } else { note });
                    return Ok(());
                }
                "list" => ("files.list", if path.is_empty() { json!({}) } else { json!({"path": path}) }),
                "read" => ("files.read", json!({"path": path})),
                "write" => {
                    if cut_files.contains(&path) {
                        log(id, format!("Step {step}: the model tried to rewrite {path}, which was too long for it to read in full. The helper didn't allow it."));
                        messages.push(ChatMessage { role: "user".into(), content: format!("Not allowed: {path} was too long for you to read in full, so you may not rewrite it. Choose another step.") });
                        continue;
                    }
                    ("files.write_staged", json!({"path": path, "text": text}))
                }
                other => {
                    log(id, format!("Step {step}: the model asked for \"{other}\", which isn't something it can do."));
                    messages.push(ChatMessage { role: "user".into(), content: format!("\"{other}\" isn't available. Use list, read, write or done.") });
                    continue;
                }
            };
            let key = format!("hb-model-{step}");
            let mut r = c
                .call("v1.actions.submit", json!({"session_id": sid, "idempotency_key": key, "operation_id": op, "arguments": args}))
                .await
                .map_err(|e| e.message)?;
            if r["state"] == "awaiting_approval" {
                log(id, format!("Step {step}: Protect's rules say this needs your OK. Answer it under \"Waiting for your OK\" on this page."));
                let action_id = r["id"].as_str().unwrap_or("").to_string();
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(APPROVAL_WAIT_SECS);
                while r["state"] == "awaiting_approval" || r["state"] == "authorized" {
                    if std::time::Instant::now() > deadline || state_of(id).as_deref() == Some("stopping") {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
                    r = c.call("v1.actions.get", json!({"action_id": action_id})).await.map_err(|e| e.message)?;
                }
            }
            let executed = r["executed"].as_bool().unwrap_or(false);
            let reason = r["reason"].as_str().unwrap_or("").to_string();
            let feedback = if !executed {
                let why = if r["state"] == "awaiting_approval" { "nobody answered in time".to_string() } else { reason.clone() };
                log(id, match op {
                    "files.list" => format!("Step {step}: Protect didn't allow listing {}: {why}", if path.is_empty() { "the folder".into() } else { path.clone() }),
                    "files.read" => format!("Step {step}: Protect didn't allow reading {path}: {why}"),
                    _ => format!("Step {step}: Protect didn't allow a change to {path}: {why}"),
                });
                format!("Protect refused that: {why}")
            } else {
                let out = &r["output"];
                match op {
                    "files.list" => {
                        let files: Vec<String> = out["files"].as_array().into_iter().flatten().map(|f| format!("{} ({} bytes)", f["path"].as_str().unwrap_or("?"), f["bytes"].as_u64().unwrap_or(0))).collect();
                        log(id, format!("Step {step}: looked at the file list ({} files).", files.len()));
                        let (t, was_cut) = cut(&files.join("\n"), read_cap);
                        format!("Files:\n{t}{}", if was_cut { "\n(list cut short)" } else { "" })
                    }
                    "files.read" => {
                        if let Some(t) = out["text"].as_str() {
                            let (shown, was_cut) = cut(t, read_cap);
                            if was_cut {
                                cut_files.insert(path.clone());
                            }
                            log(id, format!("Step {step}: read {path}."));
                            format!("Contents of {path}:\n{shown}{}", if was_cut { "\n(FILE CUT SHORT: too long to show in full. Do not rewrite this file.)" } else { "" })
                        } else {
                            cut_files.insert(path.clone());
                            log(id, format!("Step {step}: tried to read {path}, which isn't a text file."));
                            format!("{path} isn't a text file, so it can't be shown or rewritten.")
                        }
                    }
                    _ => {
                        log(id, format!("Step {step}: suggested a change to {path} (staged, not applied)."));
                        format!("Staged {path} ({} bytes). The user will review it.", text.len())
                    }
                }
            };
            messages.push(ChatMessage { role: "user".into(), content: feedback });
        }
        log(id, format!("The model used all {MAX_STEPS} of its steps, so the helper stopped it."));
        summary = Some(format!("Stopped after {MAX_STEPS} steps before the model said it was done."));
        Ok(())
    }
    .await;
    // Stop the session either way, so nothing more can be submitted on it.
    c.call("v1.sessions.stop", json!({"session_id": sid})).await.ok();
    outcome?;
    Ok(summary.unwrap_or_default())
}
