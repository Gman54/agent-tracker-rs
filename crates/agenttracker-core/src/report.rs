//! Report export (self-contained HTML / JSON) and "create a check from an incident".
use crate::event::{AgentEvent, EventType};
use crate::store::VerificationRun;
use crate::task::{FolderRole, TaskDefinition};
use crate::verifiers::{CheckResult, CheckStatus};
use serde::Serialize;
use serde_json::{json, Value};

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

fn verdict_text(v: Option<CheckStatus>) -> (&'static str, &'static str, &'static str) {
    match v {
        Some(CheckStatus::Passed) => ("passed", "\u{2713}", "All required checks passed."),
        Some(CheckStatus::Failed) => ("failed", "\u{2717}", "Something needs your attention."),
        _ => ("unsure", "?", "We couldn't confirm everything."),
    }
}

fn status_text(s: CheckStatus) -> (&'static str, &'static str) {
    match s {
        CheckStatus::Passed => ("\u{2713}", "Passed"),
        CheckStatus::Failed => ("\u{2717}", "Failed"),
        CheckStatus::UnableToVerify => ("?", "Couldn't confirm"),
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
    let entries = conversation.get("entries")?.as_array()?;
    let text = entries.iter().rev().find(|e| e.get("kind").and_then(Value::as_str) == Some("claude"))?.get("text")?.as_str()?;
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
pub fn said_vs_happened(task: &TaskDefinition, run: &VerificationRun, events: &[AgentEvent], conversation: &Value) -> Value {
    let who = if task.connector == "claude_code" { "Claude" } else { "The agent" };
    let manual = task.connector == "manual";
    let quote = if manual { None } else { last_claude_words(conversation) };
    let reported_done = events.iter().any(|e| e.event_type == EventType::AgentReportedCompletion);
    let heard = events.iter().any(|e| e.event_type != EventType::CaptureError);
    // Only a real "I'm done" signal counts as a claim; Claude's last words are quoted, not judged.
    let claimed = !manual && reported_done;

    let said = if manual {
        "No agent was listening for this task, so there's nothing it said.".to_string()
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
    let passed = run.results.iter().filter(|r| r.status == CheckStatus::Passed).count();
    let failed: Vec<String> = run.results.iter().filter(|r| r.status == CheckStatus::Failed).map(|r| check_name(&r.check_type)).collect();
    let unsure = run.results.iter().filter(|r| r.status == CheckStatus::UnableToVerify).count();
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    let happened = if total == 0 {
        "No checks were run.".to_string()
    } else if !failed.is_empty() {
        format!("{} of {total} check{} failed: {}.", failed.len(), plural(total), failed.join(", "))
    } else if unsure > 0 {
        format!("{passed} of {total} check{} passed, and {unsure} couldn't be confirmed.", plural(total))
    } else {
        format!("All {total} check{} passed.", plural(total))
    };

    let verdict = run.overall_verdict;
    let has_quote = quote.is_some();
    let (tone, headline) = match (claimed, verdict) {
        (false, Some(CheckStatus::Passed)) if has_quote => ("ok", "Here's what it said last, and the checks passed."),
        (false, Some(CheckStatus::Failed)) if has_quote => ("bad", "Here's what it said last, but the checks found a problem."),
        (false, _) if has_quote => ("warn", "Here's what it said last, but I couldn't confirm everything."),
        (true, Some(CheckStatus::Passed)) => ("ok", "It said it was done, and the checks agree."),
        (true, Some(CheckStatus::Failed)) => ("bad", "It said it was done, but the checks found a problem."),
        (true, _) => ("warn", "It said it was done, but I couldn't confirm all of it."),
        (false, Some(CheckStatus::Passed)) => ("ok", "The checks passed."),
        (false, Some(CheckStatus::Failed)) => ("bad", "The checks found a problem."),
        (false, _) => ("warn", "I couldn't confirm everything."),
    };
    json!({
        "tone": tone,
        "headline": headline,
        "said": said,
        "quote": quote,
        "claimed": claimed,
        "happened": happened,
    })
}

fn said_vs_happened_html(s: &Value) -> String {
    let g = |k: &str| s.get(k).and_then(Value::as_str).unwrap_or("");
    let cls = match g("tone") {
        "ok" => "passed",
        "bad" => "failed",
        _ => "unsure",
    };
    let quote = if g("quote").is_empty() { String::new() } else { format!(" <q>{}</q>", esc(g("quote"))) };
    format!(
        "<div class=\"card svh {cls}\"><h2>What it said vs what really happened</h2><p><b>{}</b></p><p><span class=\"m\">Said:</span> {}{quote}</p><p><span class=\"m\">Really happened:</span> {}</p></div>",
        esc(g("headline")),
        esc(g("said")),
        esc(g("happened"))
    )
}

/// JSON export: the run, the frozen task definition it was checked against, and agent events.
pub fn export_json(task: &TaskDefinition, run: &VerificationRun, events: &[AgentEvent], conversation: &Value) -> Value {
    json!({
        "format": "agenttracker-report/1",
        "generated_at": crate::now_ms(),
        "app_version": crate::VERSION,
        "note": "AgentTracker observes and reports. It does not block agents, and a passing result only means the configured checks passed.",
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
                "action" => format!("<div class=\"c act\"><b>Action</b> {tool} <span class=\"m\">{text}</span></div>"),
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

pub fn export_html(task: &TaskDefinition, run: &VerificationRun, events: &[AgentEvent], conversation: &Value) -> String {
    let (cls, icon, head) = verdict_text(run.overall_verdict);
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
        .map(|e| format!("<li>{} <span class=\"m\">{}</span></li>", esc(event_label(e.event_type)), esc(&e.connector_name)))
        .collect();
    let folders: String = task.folders.iter().map(|f| format!("<li>{} <span class=\"m\">({})</span></li>", esc(&f.folder_path), f.role.as_str())).collect();
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><title>AgentTracker report — {title}</title>
<style>body{{font:15px/1.6 "Segoe UI Variable","Segoe UI",system-ui,sans-serif;background:#f7f5f0;color:#2c2825;max-width:860px;margin:32px auto;padding:0 20px}}
.card{{background:#fff;border:1px solid #e8e2d9;border-radius:20px;padding:20px 24px;margin:16px 0}}
.verdict{{border-radius:20px;padding:22px 26px;font-size:22px;font-weight:600}}.passed{{background:#e7f6ee;color:#1a6a48}}.failed{{background:#fbeaea;color:#a83030}}.unsure,.unable_to_verify{{background:#fbf1dc;color:#8a5d08}}
.check{{border-radius:12px;padding:12px 16px;margin:10px 0}}.c{{border-radius:12px;padding:8px 14px;margin:8px 0;background:#faf8f4}}.c.you{{background:#eef3fb}}.c.bad{{background:#fbeaea}}.t{{white-space:pre-wrap;word-break:break-word}}.svh{{border-width:2px}}.svh.passed{{border-color:#bfe9d4}}.svh.failed{{border-color:#f5d0d0}}.svh.unsure{{border-color:#f0dcae}}.svh p{{margin:6px 0}}summary{{cursor:pointer}}.m,.ev,.lim{{color:#7a736b;font-size:13px}}h2{{font-size:17px}}</style></head><body>
<h1>AgentTracker report</h1><div class="verdict {cls}">{icon} {head}</div>
{svh}
<div class="card"><h2>What you asked</h2><p><b>{title}</b></p><p>{instr}</p><ul>{folders}</ul><p class="m">Task definition version {ver} &middot; checked {when}</p></div>
<div class="card"><h2>What the agent said</h2><ul>{ev}</ul></div>
<div class="card"><h2>What we saw</h2>{req}{opt}</div>
{conv}
<p class="m">AgentTracker observes and reports; it doesn't block agents or undo changes. A pass means the configured checks passed &mdash; not that the work is correct in every way.</p>
</body></html>"#,
        title = esc(&task.title),
        instr = esc(&task.instruction),
        ver = run.definition_version,
        when = run.finished_at.unwrap_or(run.started_at),
        ev = if ev_html.is_empty() { "<li class=\"m\">No agent activity was recorded (file checks only).</li>".to_string() } else { ev_html },
        req = req_html,
        opt = opt_html,
        conv = conversation_html(conversation),
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
                return not_mappable("This incident can't be turned into an automated check: AgentTracker couldn't tell which files were missing.");
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
