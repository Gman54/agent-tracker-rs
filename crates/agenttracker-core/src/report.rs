//! Report export (self-contained HTML / JSON) and "create a check from an incident".
use crate::event::{AgentEvent, EventType};
use crate::store::VerificationRun;
use crate::task::{FolderRole, TaskDefinition};
use crate::verifiers::{check_kind, CheckKind, CheckResult, CheckStatus};
use serde::Serialize;
use serde_json::{json, Value};

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

/// What the checks can honestly vouch for, as one state:
/// `not_run`, `failed`, `unable`, `passed` (a check looked at the output's content),
/// `passed_exists` (the output exists; nobody checked it's right), `safety_only` (only checks that
/// protect the originals / limit changes passed), `partial` (output checks passed, but a check had
/// insufficient coverage). Zero checks is never "couldn't confirm".
pub fn verification_state(run: &VerificationRun) -> &'static str {
    let rs = &run.results;
    if rs.is_empty() {
        return "not_run";
    }
    if run.overall_verdict == Some(CheckStatus::Failed) {
        return "failed";
    }
    let passed: Vec<&CheckResult> = rs.iter().filter(|r| r.status == CheckStatus::Passed).collect();
    let unsure = rs.iter().any(|r| r.status == CheckStatus::UnableToVerify);
    let insufficient = rs.iter().any(|r| r.status == CheckStatus::InsufficientCoverage);
    // Only "insufficient coverage" (never a real "couldn't confirm") may hold back a pass here.
    let passing = run.overall_verdict == Some(CheckStatus::Passed) || (!unsure && insufficient && !passed.is_empty());
    if !passing || passed.is_empty() {
        return "unable";
    }
    let content = passed.iter().any(|r| check_kind(&r.check_type) == CheckKind::OutputContent);
    let exists = passed.iter().any(|r| check_kind(&r.check_type) == CheckKind::OutputExists);
    match (content || exists, insufficient) {
        (false, _) => "safety_only",
        (true, true) => "partial",
        (true, false) if content => "passed",
        _ => "passed_exists",
    }
}

/// True when a passing check protected the files that were already there.
fn kept_originals(run: &VerificationRun) -> bool {
    run.results.iter().any(|r| r.status == CheckStatus::Passed && matches!(r.check_type.as_str(), "originals_unchanged" | "no_unexpected_deletions"))
}

fn safety_label(run: &VerificationRun) -> &'static str {
    if kept_originals(run) {
        "Original files kept \u{2014} the result itself wasn't checked"
    } else {
        "Safety checks passed \u{2014} the result itself wasn't checked"
    }
}

/// Headline for a verification run (HTML report).
fn verdict_text(run: &VerificationRun) -> (&'static str, &'static str, &'static str) {
    match verification_state(run) {
        "not_run" => ("neutral", "\u{2013}", "Verification wasn't run"),
        "passed" | "passed_exists" => ("passed", "\u{2713}", "All required checks passed"),
        "safety_only" => ("neutral", "\u{2013}", safety_label(run)),
        "partial" => ("neutral", "\u{2013}", "Output checks passed, but coverage was incomplete"),
        "failed" => ("failed", "\u{2717}", "A check found a problem"),
        _ => ("unsure", "?", "Unable to verify"),
    }
}

/// True when the text looks like an error from the AI service rather than a real reply
/// (Claude Code writes these into the record as if Claude said them, e.g. "API Error: 529 ...").
pub fn looks_like_api_error(text: &str) -> bool {
    let t = text.trim_start();
    let head: String = t.chars().take(300).collect::<String>().to_lowercase();
    head.starts_with("api error")
        || head.starts_with("error: api")
        || ["overloaded_error", "rate_limit_error", "authentication_error", "invalid_request_error", "\"type\":\"api_error\"", "\"type\": \"api_error\""]
            .iter()
            .any(|k| head.contains(k))
}

/// The raw text of Claude's last reply in the conversation record.
fn last_claude_text(conversation: &Value) -> Option<&str> {
    let entries = conversation.get("entries")?.as_array()?;
    entries.iter().rev().find(|e| e.get("kind").and_then(Value::as_str) == Some("claude"))?.get("text")?.as_str()
}

fn status_text(s: CheckStatus) -> (&'static str, &'static str) {
    match s {
        CheckStatus::Passed => ("\u{2713}", "Passed"),
        CheckStatus::Failed => ("\u{2717}", "Failed"),
        CheckStatus::UnableToVerify => ("?", "Couldn't confirm"),
        CheckStatus::NotApplicable => ("\u{2013}", "Not applicable"),
        CheckStatus::InsufficientCoverage => ("\u{2013}", "Insufficient coverage"),
    }
}

/// Plain-English name of an event type, as shown in reports and the UI.
pub fn event_label(t: EventType) -> &'static str {
    match t {
        EventType::SessionStarted => "Agent session started",
        EventType::TaskSubmitted => "You (or the agent) sent a request",
        EventType::ToolStarted => "Agent started using a tool",
        EventType::ToolFinished => "Agent finished using a tool",
        EventType::AgentReportedCompletion => "Agent said it's done (unverified)",
        EventType::SessionEnded => "Agent session ended",
        EventType::CaptureError => "Couldn't understand a message from the agent",
    }
}

/// One line for an event in the report list. A Cursor tool step that failed says so, with the
/// reason Cursor gave, instead of looking like a normal finished step.
pub fn event_line(e: &AgentEvent) -> String {
    let hook = e.raw_source.as_ref().and_then(|r| r.get("hook_event")).and_then(Value::as_str);
    if e.event_type == EventType::ToolFinished && hook == Some("postToolUseFailure") {
        let why = e.payload_redacted.pointer("/data/error_message").and_then(Value::as_str).map(str::trim).filter(|w| !w.is_empty());
        return match why {
            Some(w) => {
                let w: String = w.chars().take(160).collect();
                format!("Agent tried a tool and it didn't work: {w}")
            }
            None => "Agent tried a tool and it didn't work".to_string(),
        };
    }
    event_label(e.event_type).to_string()
}

/// Turns a millisecond timestamp into a readable date and time, like "2026-11-09 14:47 UTC".
pub fn friendly_time(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Days since 1970-01-01 to a calendar date (Howard Hinnant's civil_from_days).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02} UTC", rem / 3600, rem % 3600 / 60)
}

/// Friendly check name (kept in step with `CHECKS` in static/app.js).
pub fn check_name(t: &str) -> String {
    match t {
        "originals_unchanged" => "Protected files stayed the same",
        "required_outputs_exist" => "Required files exist",
        "inputs_preserved_in_output" => "Nothing got lost on the way",
        "changes_within_scope" => "Changes stayed where they should",
        "folder_structure_matches" => "The folder looks right",
        "no_unexpected_deletions" => "Nothing got deleted by surprise",
        "edits_inside_folder" => "The agent only changed files in the project folder",
        other => return other.replace('_', " "),
    }
    .to_string()
}

const QUOTE_CHARS: usize = 240;

/// Claude's last reply in the conversation record (if one was read), shortened for a one-line quote.
fn last_claude_words(conversation: &Value) -> Option<String> {
    let text = last_claude_text(conversation)?;
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.is_empty() {
        return None;
    }
    Some(match flat.char_indices().nth(QUOTE_CHARS) {
        Some((i, _)) => format!("{}\u{2026}", flat[..i].trim_end()),
        None => flat,
    })
}

/// The one-line "What it said vs what really happened" summary shown at the top of a report.
/// Purely mechanical: what the agent claimed (completion event, or Claude's last words in the
/// record) set beside the check results. It never judges whether the words were honest beyond that.
///
/// Besides the headline it returns two separate items the UI shows side by side:
/// `session` (what happened to the agent session) and `verification` (what the checks found).
pub fn said_vs_happened(task: &TaskDefinition, run: &VerificationRun, events: &[AgentEvent], conversation: &Value) -> Value {
    let who = if task.connector == "claude_code" { "Claude" } else { "The agent" };
    let manual = task.connector == "manual";
    let quote = if manual { None } else { last_claude_words(conversation) };
    let agent_error = !manual && last_claude_text(conversation).is_some_and(looks_like_api_error);
    let reported_done = events.iter().any(|e| e.event_type == EventType::AgentReportedCompletion);
    let ended = events.iter().any(|e| e.event_type == EventType::SessionEnded);
    let heard = events.iter().any(|e| e.event_type != EventType::CaptureError);
    // Only a real "I'm done" signal counts as a claim; Claude's last words are quoted, not judged.
    // An API error is never a claim, even though Claude Code still sends its "stopped" signal after one.
    let claimed = !manual && reported_done && !agent_error;

    let said = if manual {
        "No agent was listening for this task, so there's nothing it said.".to_string()
    } else if agent_error {
        format!("{who}'s last message was an error from the AI service, not a finished answer:")
    } else if reported_done && quote.is_none() {
        format!("{who} said it was done.")
    } else if reported_done {
        format!("{who} said it was done. Its last words:")
    } else if quote.is_some() {
        format!("{who}'s last words:")
    } else if heard {
        format!("{who} didn't say it was finished.")
    } else {
        format!("I didn't hear anything from {}.", if who == "Claude" { "Claude" } else { "the agent" })
    };

    let total = run.results.len();
    let count = |st: CheckStatus| run.results.iter().filter(|r| r.status == st).count();
    let passed = count(CheckStatus::Passed);
    let na = count(CheckStatus::NotApplicable);
    let insufficient: Vec<&CheckResult> = run.results.iter().filter(|r| r.status == CheckStatus::InsufficientCoverage).collect();
    let failed: Vec<String> = run.results.iter().filter(|r| r.status == CheckStatus::Failed).map(|r| check_name(&r.check_type)).collect();
    let unsure: Vec<&CheckResult> = run.results.iter().filter(|r| r.status == CheckStatus::UnableToVerify).collect();
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    let happened = if total == 0 {
        "No checks were run.".to_string()
    } else if !failed.is_empty() {
        format!("{} of {total} check{} failed: {}.", failed.len(), plural(total), failed.join(", "))
    } else if passed == total {
        if total == 1 { "The one check passed.".to_string() } else { format!("All {total} checks passed.") }
    } else {
        let mut parts = vec![];
        if passed > 0 {
            parts.push(format!("{passed} passed"));
        }
        if !insufficient.is_empty() {
            parts.push(format!("{} had insufficient coverage", insufficient.len()));
        }
        if !unsure.is_empty() {
            parts.push(format!("{} couldn't be confirmed", unsure.len()));
        }
        if na > 0 {
            parts.push(format!("{na} had nothing to check"));
        }
        let list = match parts.len() {
            1 => parts[0].clone(),
            _ => format!("{} and {}", parts[..parts.len() - 1].join(", "), parts[parts.len() - 1]),
        };
        format!("Of {total} check{}: {list}.", plural(total))
    };

    // Verification result, kept apart from the session.
    let v_state = verification_state(run);
    let not_checked = "No check looked at whether the output is complete or correct. Add checks to evaluate it.";
    let gap = insufficient.first().map(|r| format!(" {}: {}", check_name(&r.check_type), r.summary.trim())).unwrap_or_default();
    let (v_tone, v_label, v_detail) = match v_state {
        "not_run" => ("neutral", "Verification wasn't run", "Add checks to evaluate this task.".to_string()),
        "passed" => ("ok", "All required checks passed", happened.clone()),
        "passed_exists" => ("ok", "All required checks passed", format!("{happened} These checks confirm the output exists, not that it's correct.")),
        "safety_only" => ("neutral", safety_label(run), format!("{happened} {not_checked}{gap}")),
        "partial" => ("neutral", "Output checks passed, but coverage was incomplete", format!("{happened}{gap}")),
        "failed" => ("bad", "A check found a problem", happened.clone()),
        _ => {
                let reason = match unsure.first() {
                    Some(r) if !r.summary.trim().is_empty() => format!("{}: {}", check_name(&r.check_type), r.summary.trim()),
                    Some(r) => format!("{} couldn't be confirmed.", check_name(&r.check_type)),
                    None => "None of the checks are marked as required, so there's nothing to confirm against.".to_string(),
                };
                let more = if unsure.len() > 1 { format!(" ({} more couldn't be confirmed.)", unsure.len() - 1) } else { String::new() };
                ("warn", "Unable to verify", format!("{reason}{more}"))
            }
    };

    // Session status, kept apart from verification.
    let (s_state, s_tone, s_label, s_detail) = if manual {
        ("none", "neutral", "No agent session", "This task was checked by hand.".to_string())
    } else if agent_error {
        ("error", "bad", "Agent encountered an error", if ended { "The session ended after the error.".to_string() } else { "The work may not be finished.".to_string() })
    } else if ended {
        ("ended", "neutral", "Session ended", if reported_done { "The agent said it was done before the session ended.".to_string() } else { String::new() })
    } else if reported_done {
        ("finished", "neutral", "Finished", "The agent said it was done. That's its own word, not a check.".to_string())
    } else if heard {
        ("open", "neutral", "No finish signal", format!("{who} didn't say it was finished."))
    } else {
        ("silent", "neutral", "Nothing heard", format!("No activity was received from {}.", if who == "Claude" { "Claude" } else { "the agent" }))
    };

    let has_quote = quote.is_some();
    let (tone, headline) = if agent_error {
        ("bad", "Agent encountered an error".to_string())
    } else {
        let safety_tail = if kept_originals(run) { "the original files were kept, but the result itself wasn't checked" } else { "the safety checks passed, but the result itself wasn't checked" };
        let tail = match v_state {
            "not_run" => "verification wasn't run",
            "passed" | "passed_exists" => "all required checks passed",
            "safety_only" => safety_tail,
            "partial" => "the output checks passed, but coverage was incomplete",
            "failed" => "a check found a problem",
            _ => "it was unable to verify the work",
        };
        let tone = v_tone;
        // "It said it was done; the original files were kept, but ..." reads better than "but ... but".
        let joiner = match v_state {
            "passed" | "passed_exists" => ", and",
            "safety_only" | "partial" => ";",
            _ => ", but",
        };
        let h = if claimed {
            format!("It said it was done{joiner} {tail}.")
        } else if has_quote {
            format!("Here's what it said last{joiner} {tail}.")
        } else {
            let mut c = tail.chars();
            format!("{}{}.", c.next().map(|f| f.to_uppercase().collect::<String>()).unwrap_or_default(), c.as_str())
        };
        (tone, h)
    };
    json!({
        "tone": tone,
        "headline": headline,
        "said": said,
        "quote": quote,
        "claimed": claimed,
        "agent_error": agent_error,
        "happened": happened,
        "session": {"state": s_state, "tone": s_tone, "label": s_label, "detail": s_detail},
        "verification": {"state": v_state, "tone": v_tone, "label": v_label, "detail": v_detail, "checks_run": total},
        "at_a_glance": at_a_glance(task, run, events, conversation),
    })
}

/// Three plain rows for presenting a run: what was captured, what the agent explained in its own
/// words, and what was independently checked. Counts only; never a judgement of the content.
pub fn at_a_glance(task: &TaskDefinition, run: &VerificationRun, events: &[AgentEvent], conversation: &Value) -> Value {
    let plural = |n: usize, one: &str, many: &str| if n == 1 { format!("1 {one}") } else { format!("{n} {many}") };
    let (calls, matched) = crate::verifiers::tool_call_pairs(events);
    let activity = if task.connector == "manual" && events.is_empty() {
        "No agent was listening; this task was checked by hand.".to_string()
    } else if events.is_empty() {
        "Nothing was captured from the agent.".to_string()
    } else if calls == 0 {
        "Agent messages arrived, but no tool calls.".to_string()
    } else if task.connector == crate::connectors::cursor::NAME && !events.iter().any(|e| e.event_type == EventType::ToolStarted) {
        // Houndbit never registers Cursor's "before" hooks (they can block Cursor), so Cursor tool
        // calls only ever arrive once they've finished. That's expected, not a gap.
        format!("{} recorded when finished (Cursor only reports tool calls afterwards)", plural(calls, "tool call", "tool calls"))
    } else if matched == calls {
        format!("{} with matching start/end records", plural(calls, "tool call", "tool calls"))
    } else {
        format!("{}, {} without both a start and a finish record", plural(calls, "tool call", "tool calls"), calls - matched)
    };

    let explanation = if conversation.get("status").and_then(Value::as_str) != Some("ok") && task.connector == crate::connectors::cursor::NAME {
        "Cursor doesn't share what the agent wrote, so no written explanation was captured.".to_string()
    } else if conversation.get("status").and_then(Value::as_str) != Some("ok") {
        "No conversation record was available, so no written explanation was captured.".to_string()
    } else {
        let entries = conversation.get("entries").and_then(Value::as_array).cloned().unwrap_or_default();
        let kind = |e: &Value, k: &str| e.get("kind").and_then(Value::as_str) == Some(k);
        let notes = entries.iter().filter(|e| kind(e, "claude") && !e.get("text").and_then(Value::as_str).is_some_and(looks_like_api_error)).count();
        let thinking = entries.iter().filter(|e| kind(e, "thinking")).count();
        let actions = entries.iter().filter(|e| kind(e, "action")).count();
        let with_note = entries.iter().filter(|e| kind(e, "action") && e.get("note_before").is_some()).count();
        if notes == 0 && thinking == 0 {
            "Claude didn't write any explanation in this record.".to_string()
        } else {
            let mut s = format!("Claude wrote {} in its own words (not hidden thinking)", plural(notes, "explanation", "explanations"));
            if thinking > 0 {
                s.push_str(&format!(", plus {}", plural(thinking, "thinking entry", "thinking entries")));
            }
            if actions > 0 {
                s.push_str(&format!("; {with_note} of {actions} actions had a note written just before them"));
            }
            s.push('.');
            s
        }
    };

    let state = verification_state(run);
    let names = |st: CheckStatus| run.results.iter().filter(|r| r.status == st).map(|r| check_name(&r.check_type)).collect::<Vec<_>>().join(", ");
    let verification = match state {
        "not_run" => "Verification wasn't run.".to_string(),
        "failed" => format!("A check found a problem: {}.", names(CheckStatus::Failed)),
        "safety_only" if kept_originals(run) => "Original files remained; output correctness was not evaluated.".to_string(),
        "safety_only" => "Safety checks passed; output correctness was not evaluated.".to_string(),
        "passed_exists" => "The required output exists; its correctness was not evaluated.".to_string(),
        "passed" => "All required checks passed, including checks on the output itself.".to_string(),
        "partial" => format!("Output checks passed; insufficient coverage: {}.", names(CheckStatus::InsufficientCoverage)),
        _ => {
            let n = names(CheckStatus::UnableToVerify);
            if n.is_empty() { "Nothing could be confirmed.".to_string() } else { format!("Couldn't confirm: {n}.") }
        }
    };
    json!([
        {"category": "Activity capture", "result": activity},
        {"category": "Explanation captured", "result": explanation},
        {"category": "Independent verification", "result": verification},
    ])
}

fn said_vs_happened_html(s: &Value) -> String {
    let g = |k: &str| s.get(k).and_then(Value::as_str).unwrap_or("");
    let cls = match g("tone") {
        "ok" => "passed",
        "bad" => "failed",
        "neutral" => "neutral",
        _ => "unsure",
    };
    let item = |k: &str| {
        let o = s.get(k).cloned().unwrap_or(Value::Null);
        let f = |n: &str| o.get(n).and_then(Value::as_str).unwrap_or("").to_string();
        let d = f("detail");
        format!("<p><span class=\"m\">{}:</span> <b>{}</b>{}</p>", if k == "session" { "Session" } else { "Verification" }, esc(&f("label")), if d.is_empty() { String::new() } else { format!(" &mdash; {}", esc(&d)) })
    };
    let quote = if g("quote").is_empty() { String::new() } else { format!(" <q>{}</q>", esc(g("quote"))) };
    let rows: String = s
        .get("at_a_glance")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|r| format!("<tr><th>{}</th><td>{}</td></tr>", esc(r.get("category").and_then(Value::as_str).unwrap_or("")), esc(r.get("result").and_then(Value::as_str).unwrap_or(""))))
                .collect()
        })
        .unwrap_or_default();
    let glance = if rows.is_empty() { String::new() } else { format!("<table class=\"glance\"><caption>At a glance</caption>{rows}</table>") };
    format!(
        "<div class=\"card svh {cls}\"><h2>What it said vs what really happened</h2><p><b>{}</b></p>{glance}<p><span class=\"m\">Said:</span> {}{quote}</p><p><span class=\"m\">Really happened:</span> {}</p>{}{}</div>",
        esc(g("headline")),
        esc(g("said")),
        esc(g("happened")),
        item("session"),
        item("verification")
    )
}

/// JSON export: the run, the frozen task definition it was checked against, and agent events.
pub fn export_json(task: &TaskDefinition, run: &VerificationRun, events: &[AgentEvent], conversation: &Value) -> Value {
    export_json_with(task, run, events, conversation, &Value::Null)
}

/// As [`export_json`], plus the timeline from [`crate::quick_list::timeline`].
pub fn export_json_with(task: &TaskDefinition, run: &VerificationRun, events: &[AgentEvent], conversation: &Value, timeline: &Value) -> Value {
    json!({
        "timeline": timeline,
        "format": "agenttracker-report/1",
        "generated_at": crate::now_ms(),
        "app_version": crate::VERSION,
        "note": "Houndbit observes and reports. It does not block agents, and a passing result only means the configured checks passed.",
        "task": task,
        "run": run,
        "agent_events": events,
        "said_vs_happened": said_vs_happened(task, run, events, conversation),
        "conversation": conversation,
    })
}

/// Self-contained HTML (inline CSS, no scripts, everything escaped).
/// The "Conversation" card for the HTML report (thinking and long text folded with <details>).
fn conversation_html(conv: &Value) -> String {
    let status = conv.get("status").and_then(Value::as_str).unwrap_or("");
    if status.is_empty() {
        return String::new();
    }
    let entries = conv.get("entries").and_then(Value::as_array).cloned().unwrap_or_default();
    if entries.is_empty() {
        let msg = conv.get("message").and_then(Value::as_str).unwrap_or("");
        return format!("<div class=\"card\"><h2>Conversation</h2><p class=\"m\">{}</p></div>", esc(msg));
    }
    let rows: String = entries
        .iter()
        .map(|e| {
            let s = |k: &str| e.get(k).and_then(Value::as_str).unwrap_or("");
            let text = esc(s("text"));
            let cut = if e.get("cut").and_then(Value::as_bool) == Some(true) { " <span class=\"m\">(cut short here)</span>" } else { "" };
            let tool = esc(s("tool"));
            let err = e.get("is_error").and_then(Value::as_bool) == Some(true);
            match s("kind") {
                "you" => format!("<div class=\"c you\"><b>You</b><div class=\"t\">{text}{cut}</div></div>"),
                "claude" => format!("<div class=\"c cl\"><b>Claude</b><div class=\"t\">{text}{cut}</div></div>"),
                "thinking" => format!("<details class=\"c th\"><summary>Claude's thinking</summary><p class=\"m\">This is the thinking Claude chose to show. It may be a summary.</p><div class=\"t\">{text}{cut}</div></details>"),
                "action" => {
                    let note = match e.get("note_before").and_then(Value::as_str) {
                        Some(n) => format!("<p class=\"note\"><span class=\"m\">Claude's written note before this action:</span> {}</p>", esc(n)),
                        None => "<p class=\"m\">No note written before this action.</p>".to_string(),
                    };
                    let asked = e.get("instruction").and_then(Value::as_str).map(|i| format!("<p class=\"m\">In reply to: {}</p>", esc(i))).unwrap_or_default();
                    let id = e.get("tool_use_id").and_then(Value::as_str).map(|i| format!(" <span class=\"m\">({})</span>", esc(i))).unwrap_or_default();
                    format!("<div class=\"c act\"><b>Action</b> {tool} <span class=\"m\">{text}</span>{id}{note}{asked}</div>")
                }
                "result" => format!(
                    "<details class=\"c res{}\"><summary><b>Result</b> {tool}{}</summary><div class=\"t\">{text}{cut}</div></details>",
                    if err { " bad" } else { "" },
                    if err { " &mdash; it reported a problem" } else { "" }
                ),
                _ => String::new(),
            }
        })
        .collect();
    let path = conv.get("path").and_then(Value::as_str).map(|p| format!("<p class=\"m\">From Claude Code's record: {}</p>", esc(p))).unwrap_or_default();
    format!("<div class=\"card\"><h2>Conversation</h2>{path}{rows}</div>")
}

/// The name to show: the saved title, or a short one made from it when it's a long sentence.
pub fn display_title(task: &TaskDefinition) -> String {
    if task.title.chars().count() <= 60 {
        return task.title.clone();
    }
    let t = crate::task::short_title(&task.title);
    if t.is_empty() { task.title.clone() } else { t }
}

/// The "When things happened" card: request, quick name list, full snapshot, first action.
pub fn timeline_html(t: &Value) -> String {
    let rows = match t.get("rows").and_then(Value::as_array) {
        Some(r) if !r.is_empty() => r,
        _ => return String::new(),
    };
    let body: String = rows
        .iter()
        .map(|r| {
            let at = r.get("at").and_then(Value::as_i64).map(friendly_time_secs).unwrap_or_else(|| "Not known".into());
            let text = r.get("text").and_then(Value::as_str).filter(|s| !s.is_empty()).map(|s| format!(" <span class=\"m\">{}</span>", esc(s))).unwrap_or_default();
            format!("<tr><th>{}</th><td>{}{}</td></tr>", esc(r.get("label").and_then(Value::as_str).unwrap_or("")), esc(&at), text)
        })
        .collect();
    let note = t.get("note").and_then(Value::as_str).map(|n| format!("<p class=\"note\"><b>{}</b></p>", esc(n))).unwrap_or_default();
    format!("<div class=\"card\"><h2>When things happened</h2><table class=\"glance\">{body}</table>{note}</div>")
}

/// Like [`friendly_time`] but with seconds, for the timeline where seconds matter.
pub fn friendly_time_secs(ms: i64) -> String {
    let base = friendly_time(ms);
    let secs = ms.div_euclid(1000).rem_euclid(60);
    base.replacen(" UTC", &format!(":{secs:02} UTC"), 1)
}

pub fn export_html(task: &TaskDefinition, run: &VerificationRun, events: &[AgentEvent], conversation: &Value) -> String {
    export_html_with(task, run, events, conversation, &Value::Null)
}

/// As [`export_html`], plus the "When things happened" card.
pub fn export_html_with(task: &TaskDefinition, run: &VerificationRun, events: &[AgentEvent], conversation: &Value, timeline: &Value) -> String {
    let (cls, icon, head) = verdict_text(run);
    let required: std::collections::HashMap<&str, bool> = task.checks.iter().map(|c| (c.id.as_str(), c.is_required)).collect();
    let card = |r: &CheckResult| {
        let (i, t) = status_text(r.status);
        let lim = if r.limitations.is_empty() { String::new() } else { format!("<p class=\"lim\">Limitations: {}</p>", esc(&r.limitations.join(" \u{00b7} "))) };
        let ev = if r.evidence_refs.is_empty() { String::new() } else { format!("<p class=\"ev\">Evidence: {}</p>", esc(&r.evidence_refs.join(", "))) };
        format!("<div class=\"check {}\"><b>{} {}</b> &mdash; {}<p>{}</p>{ev}{lim}</div>", r.status.as_str(), i, t, esc(&r.check_type.replace('_', " ")), esc(&r.summary))
    };
    let (req, opt): (Vec<_>, Vec<_>) = run.results.iter().partition(|r| *required.get(r.check_id.as_str()).unwrap_or(&true));
    let req_html: String = req.iter().map(|r| card(r)).collect();
    let opt_html = if opt.is_empty() { String::new() } else { format!("<h2>Optional checks</h2>{}", opt.iter().map(|r| card(r)).collect::<String>()) };
    let ev_html: String = events
        .iter()
        .map(|e| format!("<li>{} <span class=\"m\">{}</span></li>", esc(&event_line(e)), esc(&e.connector_name)))
        .collect();
    let folders: String = task.folders.iter().map(|f| format!("<li>{} <span class=\"m\">({})</span></li>", esc(&f.folder_path), f.role.as_str())).collect();
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><title>Houndbit report — {title}</title>
<style>body{{font:15px/1.6 "Segoe UI Variable","Segoe UI",system-ui,sans-serif;background:#f7f5f0;color:#2c2825;max-width:860px;margin:32px auto;padding:0 20px}}
.card{{background:#fff;border:1px solid #e8e2d9;border-radius:20px;padding:20px 24px;margin:16px 0}}
.verdict{{border-radius:20px;padding:22px 26px;font-size:22px;font-weight:600}}.passed{{background:#e7f6ee;color:#1a6a48}}.failed{{background:#fbeaea;color:#a83030}}.unsure,.unable_to_verify{{background:#fbf1dc;color:#8a5d08}}.neutral,.not_applicable,.insufficient_coverage{{background:#eef0f3;color:#3f4650}}
.glance{{border-collapse:collapse;width:100%;margin:10px 0}}.glance caption{{text-align:left;font-weight:600;padding-bottom:4px}}.glance th,.glance td{{text-align:left;vertical-align:top;padding:6px 10px;border-top:1px solid #eee7dc}}.glance th{{width:34%;font-weight:600}}.note{{margin:4px 0}}
.check{{border-radius:12px;padding:12px 16px;margin:10px 0}}.c{{border-radius:12px;padding:8px 14px;margin:8px 0;background:#faf8f4}}.c.you{{background:#eef3fb}}.c.bad{{background:#fbeaea}}.t{{white-space:pre-wrap;word-break:break-word}}.svh{{border-width:2px}}.svh.passed{{border-color:#bfe9d4}}.svh.failed{{border-color:#f5d0d0}}.svh.unsure{{border-color:#f0dcae}}.svh.neutral{{border-color:#d9dde3}}.svh p{{margin:6px 0}}summary{{cursor:pointer}}.m,.ev,.lim{{color:#7a736b;font-size:13px}}h2{{font-size:17px}}</style></head><body>
<h1>Houndbit report</h1><div class="verdict {cls}">{icon} {head}</div>
{svh}
<div class="card"><h2>What you asked</h2><p><b>{title}</b></p><p>{instr}</p><ul>{folders}</ul><p class="m">Task definition version {ver} &middot; checked {when}</p></div>
<div class="card"><h2>What the agent said</h2><ul>{ev}</ul></div>
<div class="card"><h2>What we saw</h2>{req}{opt}</div>
{times}
{conv}
<p class="m">Houndbit observes and reports; it doesn't block agents or undo changes. A pass means the configured checks passed &mdash; not that the work is correct in every way.</p>
</body></html>"#,
        title = esc(&display_title(task)),
        instr = esc(&task.instruction),
        ver = run.definition_version,
        when = friendly_time(run.finished_at.unwrap_or(run.started_at)),
        ev = if ev_html.is_empty() { "<li class=\"m\">No agent activity was recorded (file checks only).</li>".to_string() } else { ev_html },
        req = req_html,
        opt = opt_html,
        conv = conversation_html(conversation),
        times = timeline_html(timeline),
        svh = said_vs_happened_html(&said_vs_happened(task, run, events, conversation)),
    )
}

#[derive(Debug, Clone, Serialize)]
pub struct IncidentProposal {
    pub mappable: bool,
    pub explanation: String,
    pub name: String,
    pub check_type: String,
    pub params: Value,
}

fn not_mappable(why: &str) -> IncidentProposal {
    IncidentProposal { mappable: false, explanation: why.into(), name: String::new(), check_type: String::new(), params: Value::Null }
}

/// Turns a failed check into a proposed saved check. Nothing is saved here — the UI shows the
/// proposal for review first.
pub fn propose_from_incident(task: &TaskDefinition, result: &CheckResult) -> IncidentProposal {
    match result.status {
        CheckStatus::Passed => return not_mappable("This check passed, so there's no incident to turn into a rule."),
        CheckStatus::NotApplicable | CheckStatus::InsufficientCoverage => {
            return not_mappable("This check didn't find a problem (there was nothing for it to look at, or it couldn't see everything), so there's no incident to turn into a rule.")
        }
        CheckStatus::UnableToVerify => {
            return not_mappable("This incident can't be turned into an automated check: the problem was missing or unreadable evidence, not a rule being broken.")
        }
        CheckStatus::Failed => {}
    }
    let original = task.checks.iter().find(|c| c.id == result.check_id).map(|c| c.params.clone()).unwrap_or(Value::Null);
    let items: Vec<&str> = result.summary.split("; ").filter(|s| !s.starts_with("and ")).collect();
    let (name, params, explanation) = match result.check_type.as_str() {
        "originals_unchanged" => {
            let mut paths: Vec<String> = original.get("paths").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
            if paths.is_empty() {
                paths = task.folders_with(FolderRole::Protected);
            }
            if paths.is_empty() {
                return not_mappable("This incident can't be turned into an automated check: the task had no protected folders to watch.");
            }
            ("Keep these originals unchanged".to_string(), json!({"paths": paths, "allow_additions": false}), format!("A protected file was changed ({}). This rule fails whenever anything in these folders is modified, deleted or added.", items.join("; ")))
        }
        "required_outputs_exist" => {
            let missing: Vec<String> = items.iter().filter_map(|s| s.strip_suffix(" is missing")).map(str::to_string).collect();
            if missing.is_empty() {
                return not_mappable("This incident can't be turned into an automated check: Houndbit couldn't tell which files were missing.");
            }
            ("These outputs must exist".to_string(), json!({"paths": missing, "min_size_bytes": 1}), "These files were missing. This rule fails if any of them is missing or empty next time.".to_string())
        }
        "changes_within_scope" | "folder_structure_matches" | "inputs_preserved_in_output" => {
            if original.is_null() {
                return not_mappable("This incident can't be turned into an automated check: the original rule's settings weren't found.");
            }
            (format!("Same rule as \u{201c}{}\u{201d}", task.title), original, format!("Reuses the exact rule that caught this: {}", items.join("; ")))
        }
        _ => return not_mappable("This incident can't be turned into an automated check."),
    };
    IncidentProposal { mappable: true, explanation, name, check_type: result.check_type.clone(), params }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::Lifecycle;

    pub(super) fn task(connector: &str) -> TaskDefinition {
        TaskDefinition {
            id: "t".into(),
            title: "Please create summary files for every report in the folder and keep the originals safe".into(),
            instruction: "x".into(),
            connector: connector.into(),
            definition_version: 1,
            lifecycle: Lifecycle::Completed,
            folders: vec![],
            checks: vec![],
            session_id: None,
            created_at: 0,
            updated_at: 0,
            origin: "auto".into(),
        }
    }
    pub(super) fn run(results: Vec<CheckResult>, v: Option<CheckStatus>) -> VerificationRun {
        VerificationRun { id: "r".into(), task_id: "t".into(), definition_version: 1, started_at: 0, finished_at: Some(1), overall_verdict: v, results }
    }
    fn result(status: CheckStatus, summary: &str) -> CheckResult {
        CheckResult { check_id: "c".into(), check_type: "required_outputs_exist".into(), check_version: "1".into(), status, summary: summary.into(), evidence_refs: vec![], limitations: vec![], started_at: 0, finished_at: 0 }
    }

    #[test]
    fn zero_checks_is_not_run() {
        let r = run(vec![], Some(CheckStatus::UnableToVerify));
        let s = said_vs_happened(&task("claude_code"), &r, &[], &Value::Null);
        assert_eq!(s["verification"]["label"], "Verification wasn't run");
        assert_eq!(s["verification"]["detail"], "Add checks to evaluate this task.");
        assert_eq!(s["headline"], "Verification wasn't run.");
        assert_eq!(s["tone"], "neutral");
        assert_eq!(verdict_text(&r).2, "Verification wasn't run");
        let html = export_html(&task("claude_code"), &r, &[], &Value::Null);
        assert!(!html.contains("couldn't confirm everything"));
        assert!(html.contains("Create summary files for every report"), "short title in the heading");
    }

    #[test]
    fn api_error_is_reported_as_error() {
        let conv = json!({"entries": [{"kind": "you", "text": "do it"}, {"kind": "claude", "text": "API Error: 500 {\"type\":\"error\",\"error\":{\"type\":\"api_error\"}}"}]});
        let r = run(vec![], None);
        let s = said_vs_happened(&task("claude_code"), &r, &[], &conv);
        assert_eq!(s["agent_error"], true);
        assert_eq!(s["claimed"], false);
        assert_eq!(s["headline"], "Agent encountered an error");
        assert_eq!(s["session"]["label"], "Agent encountered an error");
        assert_eq!(s["verification"]["label"], "Verification wasn't run");
        assert!(!s["said"].as_str().unwrap().contains("said it was done"));
        assert!(looks_like_api_error("API Error: Request timed out."));
        assert!(!looks_like_api_error("I fixed the API error in login.rs and the tests pass."));
    }

    #[test]
    fn unable_to_verify_gives_the_reason() {
        let r = run(vec![result(CheckStatus::UnableToVerify, "The output folder couldn't be read")], Some(CheckStatus::UnableToVerify));
        let s = said_vs_happened(&task("claude_code"), &r, &[], &Value::Null);
        assert_eq!(s["verification"]["label"], "Unable to verify");
        assert_eq!(s["verification"]["detail"], "Required files exist: The output folder couldn't be read");
        assert_eq!(s["headline"], "It was unable to verify the work.");
        let ok = run(vec![result(CheckStatus::Passed, "ok")], Some(CheckStatus::Passed));
        assert_eq!(said_vs_happened(&task("claude_code"), &ok, &[], &Value::Null)["verification"]["label"], "All required checks passed");
    }

    fn typed(t: &str, status: CheckStatus, summary: &str) -> CheckResult {
        CheckResult { check_type: t.into(), ..result(status, summary) }
    }

    #[test]
    fn safety_only_pass_says_the_result_wasnt_checked() {
        // The demo run: originals kept, PDF made by a shell command, no output checks.
        let rs = vec![
            typed("no_unexpected_deletions", CheckStatus::Passed, "All 17895 file(s) that were there before are still there."),
            typed("edits_inside_folder", CheckStatus::InsufficientCoverage, "Insufficient coverage: the agent ran 3 shell command(s)."),
        ];
        let r = run(rs, Some(CheckStatus::UnableToVerify));
        assert_eq!(verification_state(&r), "safety_only");
        let s = said_vs_happened(&task("claude_code"), &r, &[], &Value::Null);
        assert_eq!(s["tone"], "neutral");
        assert_eq!(s["verification"]["label"], "Original files kept \u{2014} the result itself wasn't checked");
        assert_eq!(s["headline"], "The original files were kept, but the result itself wasn't checked.");
        assert_eq!(s["happened"], "Of 2 checks: 1 passed and 1 had insufficient coverage.");
        assert_eq!(s["at_a_glance"][2]["result"], "Original files remained; output correctness was not evaluated.");
        assert_eq!(s["at_a_glance"][1]["result"], "No conversation record was available, so no written explanation was captured.");
        let html = export_html(&task("claude_code"), &r, &[], &Value::Null);
        assert!(html.contains("Insufficient coverage"));
        assert!(html.contains("At a glance"));
        assert!(!html.contains("All required checks passed"));
        // Existence-only checks keep "passed" but say what they don't cover.
        let ex = run(vec![result(CheckStatus::Passed, "ok")], Some(CheckStatus::Passed));
        let s = said_vs_happened(&task("claude_code"), &ex, &[], &Value::Null);
        assert!(s["verification"]["detail"].as_str().unwrap().contains("exists, not that it's correct"));
    }

    #[test]
    fn glance_counts_calls_and_notes() {
        let conv = json!({"status": "ok", "entries": [
            {"kind": "you", "text": "make a pdf"},
            {"kind": "claude", "text": "I'll use per-capita GDP."},
            {"kind": "action", "text": "python make.py", "note_before": "I'll use per-capita GDP."},
            {"kind": "result", "text": "ok"},
            {"kind": "action", "text": "ls"},
            {"kind": "claude", "text": "Done."}
        ]});
        let r = run(vec![], None);
        let g = at_a_glance(&task("claude_code"), &r, &[], &conv);
        assert_eq!(g[0]["result"], "Nothing was captured from the agent.");
        assert_eq!(g[1]["result"], "Claude wrote 2 explanations in its own words (not hidden thinking); 1 of 2 actions had a note written just before them.");
        assert_eq!(g[2]["result"], "Verification wasn't run.");
        let html = conversation_html(&conv);
        assert!(html.contains("Claude's written note before this action:"));
        assert!(html.contains("No note written before this action."));
    }
}

#[cfg(test)]
mod time_and_event_tests {
    use super::*;

    #[test]
    fn timeline_card_shows_times_and_the_early_start_note() {
        use crate::quick_list::{timeline, QuickList, EARLY_START_NOTE};
        let q = QuickList { received_at: 1_700_000_000_000, started_at: Some(1_700_000_000_010), finished_at: Some(1_700_000_000_400), file_count: 17904, ..Default::default() };
        let t = timeline(Some(&q), Some((1_700_000_000_500, 1_700_000_090_000)), Some(1_700_000_005_000));
        let html = export_html_with(&super::tests::task("claude_code"), &super::tests::run(vec![], None), &[], &Value::Null, &t);
        assert!(html.contains("When things happened"));
        assert!(html.contains("Your request arrived"));
        assert!(html.contains("Quick name list taken"));
        assert!(html.contains("17,904 files listed"));
        assert!(html.contains("Full &quot;before&quot; snapshot finished") || html.contains("Full \"before\" snapshot finished"));
        assert!(html.contains("The agent&#39;s first action") || html.contains("The agent's first action"));
        assert!(html.contains(&super::esc(EARLY_START_NOTE)));
        assert!(html.contains("2023-11-14 22:13:20 UTC"), "seconds are shown");
        assert_eq!(export_json_with(&super::tests::task("claude_code"), &super::tests::run(vec![], None), &[], &Value::Null, &t)["timeline"]["agent_started_early"], true);
        assert!(!export_html(&super::tests::task("claude_code"), &super::tests::run(vec![], None), &[], &Value::Null).contains("When things happened"));
    }

    #[test]
    fn friendly_time_reads_like_a_date() {
        assert_eq!(friendly_time(0), "1970-01-01 00:00 UTC");
        assert_eq!(friendly_time(1_794_235_666_264), "2026-11-09 14:47 UTC");
        assert_eq!(friendly_time(951_782_400_000), "2000-02-29 00:00 UTC");
    }
}
