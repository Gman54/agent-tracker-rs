//! Houndbit and Houndbit Protect side by side (local contract test, no live agent).
//!
//! Checks that Houndbit can use Protect the same way any other app would: through the public
//! pipe/socket, with its own pairing key, and without either program touching the other's
//! records. Also checks Houndbit keeps working when Protect isn't running. Protect still
//! decides and records only: nothing here is executed by Protect.
use agenttracker_acceptance::{claude_envelope, Fixture};
use agenttracker_core::verifiers::CheckStatus;
use protect_engine::client::{load_or_create_key, public_key_b64, Client};
use protect_engine::{transport, Engine};
use serde_json::json;
use std::sync::Arc;

const P: u64 = protect_engine::PROTOCOL_MAJOR;

fn tables(db: &std::path::Path) -> Vec<String> {
    let c = rusqlite::Connection::open(db).unwrap();
    let mut s = c.prepare("SELECT name FROM sqlite_master WHERE type='table'").unwrap();
    let v = s.query_map([], |r| r.get::<_, String>(0)).unwrap().map(Result::unwrap).collect();
    v
}

fn endpoint(dir: &std::path::Path) -> String {
    if cfg!(windows) {
        format!(r"\\.\pipe\houndbit-protect-plugin-{}", uuid::Uuid::new_v4().simple())
    } else {
        dir.join("engine.sock").to_string_lossy().into_owned()
    }
}

async fn wait_until_up(ep: &str) {
    for _ in 0..100 {
        if Client::connect(ep).await.is_ok() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("Protect engine never came up on {ep}");
}

#[tokio::test(flavor = "multi_thread")]
async fn houndbit_plugs_into_protect_as_an_ordinary_app() {
    // Houndbit, with its own data folder and database.
    let fx = Fixture::new();
    // Protect, in a different folder, served on its own pipe/socket.
    let protect_dir = tempfile::tempdir().unwrap();
    let ep = endpoint(protect_dir.path());
    let engine = Arc::new(Engine::open(protect_dir.path()).unwrap());
    let ep2 = ep.clone();
    let server = tokio::spawn(async move {
        let _ = transport::serve(engine, &ep2).await;
    });
    wait_until_up(&ep).await;
    // Stands in for the Protect window: a second handle on Protect's own records.
    let admin = Engine::open(protect_dir.path()).unwrap();

    // Houndbit keeps its pairing key in its own data folder.
    let key_file = fx.dir.path().join("protect").join("houndbit-app.key");
    let key = load_or_create_key(&key_file).unwrap();
    let mut c = Client::connect(&ep).await.unwrap();
    let r = c.call("v1.pairing.request", json!({"protocol_major": P, "app_id": "com.houndbit.app", "display_name": "Houndbit", "public_key": public_key_b64(&key), "scopes": ["observer", "requester"]})).await.unwrap();
    let pid = r["pairing_id"].as_str().unwrap().to_string();
    assert_eq!(c.call("v1.pairing.status", json!({"protocol_major": P, "pairing_id": pid})).await.unwrap()["state"], "pending");
    admin.admin_pairing_resolve(&pid, true, None).unwrap();
    assert_eq!(c.call("v1.pairing.status", json!({"protocol_major": P, "pairing_id": pid})).await.unwrap()["state"], "accepted");

    let work = fx.workspace(&[("a.txt", "alpha")]);
    admin.admin_grant_add("com.houndbit.app", work.to_str().unwrap()).unwrap();

    // "Houndbit restarts": the key comes back from disk and still signs in, no re-pairing.
    drop(c);
    let key_again = load_or_create_key(&key_file).unwrap();
    assert_eq!(key_again.to_bytes(), key.to_bytes());
    let mut c = Client::connect(&ep).await.unwrap();
    let who = c.sign_in("com.houndbit.app", &key_again).await.unwrap();
    assert_eq!(who["scopes"], json!(["observer", "requester"]), "Houndbit gets no approver rights");

    // Houndbit finds its folder on its own, then asks for something and reads the record back.
    let hello = c.call("v1.hello", json!({"protocol_major": P})).await.unwrap();
    assert!(hello["supported_features"].as_array().unwrap().contains(&json!("grants")));
    let grants = c.call("v1.grants.list", json!({})).await.unwrap();
    let grant = grants["grants"][0]["grant_id"].as_str().unwrap().to_string();
    assert_eq!(grants["grants"].as_array().unwrap().len(), 1);
    let s = c.call("v1.sessions.request", json!({"resource_grant_id": grant})).await.unwrap()["id"].as_str().unwrap().to_string();
    c.call("v1.sessions.start", json!({"session_id": s})).await.unwrap();
    let a = c.call("v1.actions.submit", json!({"session_id": s, "idempotency_key": "hb-1", "operation_id": "files.read", "arguments": {"path": "a.txt"}})).await.unwrap();
    assert_eq!((a["decision"].as_str(), a["executed"].as_bool()), (Some("allow"), Some(false)), "decided, not run");
    let ev = c.call("v1.events.read", json!({"session_id": s})).await.unwrap();
    assert!(ev["events"].as_array().unwrap().iter().any(|e| e["type"] == "action.not_executed"));

    // Meanwhile Houndbit does its normal job, unaffected.
    let task = fx.copy_task(&work, &["a.txt"], None);
    fx.engine.capture_baseline(&task.id).unwrap();
    fx.engine.start(&task.id).unwrap();
    std::fs::copy(work.join("input/a.txt"), work.join("output/a.txt")).unwrap();
    fx.deliver(claude_envelope("Stop", 1, json!({"session_id": "s-side", "cwd": work.to_string_lossy()})));
    assert_eq!(fx.engine.verify(&task.id).unwrap().overall_verdict, Some(CheckStatus::Passed));

    // Separate records: neither database has the other's tables.
    let hb = tables(&fx.dir.path().join("agenttracker.db"));
    let pr = tables(&protect_dir.path().join("protect-engine.db"));
    assert!(hb.contains(&"tasks".to_string()) && !hb.iter().any(|t| t == "pairing_requests" || t == "resource_grants"), "{hb:?}");
    assert!(pr.contains(&"pairing_requests".to_string()) && !pr.iter().any(|t| t == "tasks"), "{pr:?}");

    // Protect goes away: Houndbit gets a plain connection error (never an "allow"), and its
    // own checks still work.
    server.abort();
    let _ = server.await;
    drop(c);
    if !cfg!(windows) {
        let _ = std::fs::remove_file(&ep);
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(Client::connect(&ep).await.is_err(), "connecting to a stopped engine must fail");
    let t2 = fx.copy_task(&work, &["a.txt"], None);
    fx.engine.capture_baseline(&t2.id).unwrap();
    fx.engine.start(&t2.id).unwrap();
    assert_eq!(fx.engine.verify(&t2.id).unwrap().overall_verdict, Some(CheckStatus::Passed));
}

#[test]
fn a_damaged_key_file_is_reported_not_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("app.key");
    std::fs::write(&f, "not a key").unwrap();
    assert!(load_or_create_key(&f).is_err());
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "not a key", "the file was left alone");
}
