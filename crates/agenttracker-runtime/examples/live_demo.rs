//! LIVE-MODEL run of Demo A and Demo B against a local OpenAI-compatible endpoint.
//!
//! Usage:
//!   cargo run -p agenttracker-runtime --example live_demo -- <endpoint> <model> <db_path> [a|b|both] [investigate]
//!   e.g. cargo run -p agenttracker-runtime --example live_demo -- http://127.0.0.1:11434/v1 qwen2.5:1.5b live.db both
//!
//! Prints what actually happened (no fixtures). Results are stored in <db_path>.
use agenttracker_core::db::Db;
use agenttracker_runtime::config::{ProviderConfig, ProviderKind, RuntimeConfig};
use agenttracker_runtime::demo::{self, Demo};
use agenttracker_runtime::investigate::{self, InvestigationSpec, Modification};
use agenttracker_runtime::provider::OpenAiCompatProvider;
use agenttracker_runtime::runtime::Runtime;
use agenttracker_runtime::store;
use std::sync::Arc;
use std::time::Instant;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let endpoint = a.get(1).cloned().unwrap_or_else(|| "http://127.0.0.1:11434/v1".into());
    let model = a.get(2).cloned().unwrap_or_else(|| "qwen2.5:1.5b".into());
    let db_path = a.get(3).cloned().unwrap_or_else(|| "live_demo.db".into());
    let which = a.get(4).cloned().unwrap_or_else(|| "both".into());
    let do_inv = a.get(5).map(|s| s == "investigate").unwrap_or(false);

    let mut cfg = RuntimeConfig::default();
    cfg.provider = ProviderConfig { endpoint: endpoint.clone(), model: model.clone(), kind: if endpoint.contains("11434") { ProviderKind::Ollama } else { ProviderKind::OpenAiCompatible }, timeout_secs: 900, seed: Some(7), ..cfg.provider.clone() };
    let provider = Arc::new(OpenAiCompatProvider::new(cfg.provider.clone()).expect("provider config"));
    let db = Db::open(std::path::Path::new(&db_path)).expect("db");
    let rt = Runtime::new(db.clone(), provider.clone(), cfg, None);
    let base = std::env::temp_dir().join("agenttracker-live-demos");
    std::fs::create_dir_all(&base).unwrap();

    let demos: Vec<Demo> = match which.as_str() { "a" => vec![Demo::A], "b" => vec![Demo::B], _ => vec![Demo::A, Demo::B] };
    for d in demos {
        let (dir, spec) = demo::prepare(&base, d).unwrap();
        println!("=== LIVE {:?} in {} ===", d, dir.display());
        let out = rt.start_task(&spec).unwrap();
        let t = Instant::now();
        let mut st = rt.run(&out.session_id);
        // Demo B never needs approval (copy_file); if a write is proposed for an unprotected path we stop here.
        if let Ok(s) = &st { if s.status == "awaiting_approval" { println!("(paused for approval; not auto-approving in live test)"); } }
        let secs = t.elapsed().as_secs();
        match &mut st { Ok(s) => println!("status={} reason={:?} elapsed={}s", s.status, s.reason, secs), Err(e) => println!("ERROR {e} elapsed={secs}s") }
        let conn = db.conn();
        let sid = &out.session_id;
        for r in store::rows(&conn, "SELECT id, step, attempt, valid, errors, raw_redacted FROM rt_proposal_attempts WHERE session_id=?1 ORDER BY created_at", &[sid as &dyn rusqlite::ToSql]).unwrap() {
            let raw = r["raw_redacted"].as_str().unwrap_or("");
            println!("step {} attempt {} valid={} errors={} raw={}", r["step"], r["attempt"], r["valid"], r["errors"], &raw.chars().take(600).collect::<String>());
        }
        for r in store::rows(&conn, "SELECT id, kind, tool, args, proposal FROM rt_decisions WHERE session_id=?1 ORDER BY created_at", &[sid as &dyn rusqlite::ToSql]).unwrap() {
            println!("decision {} kind={} tool={} args={} refs={}/{} stated_rationale={}", r["id"], r["kind"], r["tool"], r["args"], r["proposal"]["instruction_refs"], r["proposal"]["evidence_refs"], r["proposal"]["stated_rationale"]);
        }
        for r in store::rows(&conn, "SELECT a.id, a.tool, (SELECT state FROM rt_action_states s WHERE s.action_id=a.id ORDER BY seq DESC LIMIT 1) AS state FROM rt_actions a WHERE a.session_id=?1", &[sid as &dyn rusqlite::ToSql]).unwrap() {
            println!("action {} tool={} final_state={}", r["id"], r["tool"], r["state"]);
        }
        let files: Vec<String> = walk(&dir).into_iter().map(|p| p.strip_prefix(&dir).unwrap().to_string_lossy().into_owned()).collect();
        println!("files after: {files:?}");
        println!("original preserved: {}", std::fs::read(dir.join("originals/photo.jpg")).map(|b| b.len()).unwrap_or(0) > 0);
        drop(conn);

        if do_inv && d == Demo::B {
            let conn = db.conn();
            let first = store::rows(&conn, "SELECT id FROM rt_decisions WHERE session_id=?1 AND kind='action' ORDER BY created_at LIMIT 1", &[sid as &dyn rusqlite::ToSql]).unwrap();
            let ev = store::rows(&conn, "SELECT id FROM rt_context_items WHERE session_id=?1 AND label='evidence_1'", &[sid as &dyn rusqlite::ToSql]).unwrap();
            drop(conn);
            if let (Some(dec), Some(item)) = (first.first(), ev.first()) {
                let t = Instant::now();
                let spec = InvestigationSpec { decision_id: dec["id"].as_str().unwrap().into(), context_item_id: item["id"].as_str().unwrap().into(), modification: Modification::Remove, trials_per_arm: 3, seed_base: Some(100) };
                match investigate::run(&db, provider.as_ref() as &dyn agenttracker_runtime::provider::Provider, None, &spec) {
                    Ok(id) => {
                        let r = store::row(&db.conn(), "SELECT status, replay_fidelity, confounded, confound_detail, summary, counts, limitations FROM rt_investigations WHERE id=?1", &[&id as &dyn rusqlite::ToSql]).unwrap();
                        println!("investigation {id} ({}s): {}", t.elapsed().as_secs(), r.unwrap_or_default());
                    }
                    Err(e) => println!("investigation error: {e}"),
                }
            } else {
                println!("investigation skipped: no accepted action decision in Demo B");
            }
        }
    }
}

fn walk(d: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut v = Vec::new();
    for e in std::fs::read_dir(d).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() { v.extend(walk(&p)) } else { v.push(p) }
    }
    v.sort();
    v
}
