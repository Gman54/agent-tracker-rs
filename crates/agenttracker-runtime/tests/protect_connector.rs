//! Houndbit's Protect connector driven against a real engine.
//!
//! Walks the whole path a user would: status when Protect is off, status before pairing, pair,
//! accept in Protect, status after, see the granted folder, then run the staged sandbox and
//! confirm the original file is never written. Everything goes through
//! `agenttracker_runtime::protect`, the ordinary client path — the test never reaches into the
//! engine's internals except on the admin handle that stands in for the Protect window.
//!
//! One test only: the connector reads its endpoint from a process-global env var, so splitting
//! this across parallel tests would race on that var.
use agenttracker_runtime::protect;
use protect_engine::{transport, Engine};
use std::sync::Arc;

/// A local endpoint for the engine: a per-run named pipe on Windows, a socket file on Unix.
/// `tag` keeps the "absent" endpoint and the real one from ever colliding.
fn endpoint(dir: &std::path::Path, tag: &str) -> String {
    if cfg!(windows) {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!(r"\\.\pipe\houndbit-protect-conn-{tag}-{}-{n}", std::process::id())
    } else {
        dir.join(format!("{tag}.sock")).to_string_lossy().into_owned()
    }
}

async fn wait_until_up(ep: &str) {
    for _ in 0..100 {
        if protect_engine::client::Client::connect(ep).await.is_ok() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("Protect engine never came up on {ep}");
}

#[tokio::test(flavor = "multi_thread")]
async fn houndbit_connector_pairs_and_runs_the_sandbox() {
    let home = tempfile::tempdir().unwrap();

    // Protect is off: status is calm, and pairing is a plain error, never a silent success.
    let absent = endpoint(home.path(), "absent");
    std::env::set_var("HOUNDBIT_PROTECT_ENDPOINT", &absent);
    let off = protect::status(home.path()).await;
    assert_eq!(off["running"], false);
    assert_eq!(off["paired"], false);
    assert!(off["message"].as_str().unwrap().contains("isn't answering"));
    assert!(protect::pair(home.path()).await.is_err());

    // Now start a real Protect engine and point the connector at it.
    let protect_dir = tempfile::tempdir().unwrap();
    let ep = endpoint(protect_dir.path(), "engine");
    std::env::set_var("HOUNDBIT_PROTECT_ENDPOINT", &ep);
    let engine = Arc::new(Engine::open(protect_dir.path()).unwrap());
    let ep2 = ep.clone();
    let server = tokio::spawn(async move {
        let _ = transport::serve(engine, &ep2).await;
    });
    wait_until_up(&ep).await;
    // Stands in for the Protect window.
    let admin = Engine::open(protect_dir.path()).unwrap();

    // Before pairing: the engine is running but Houndbit isn't connected.
    let before = protect::status(home.path()).await;
    assert_eq!(before["running"], true);
    assert_eq!(before["paired"], false);

    // Pair, then the user accepts inside Protect.
    let p = protect::pair(home.path()).await.unwrap();
    let pid = p["pairing_id"].as_str().unwrap().to_string();
    assert_eq!(p["state"], "pending");
    assert!(protect::key_path(home.path()).exists(), "pairing creates Houndbit's key");
    admin.admin_pairing_resolve(&pid, true, None).unwrap();

    // After accepting: connected, with only the scopes an app may ask for.
    let after = protect::status(home.path()).await;
    assert_eq!(after["running"], true);
    assert_eq!(after["paired"], true);
    assert_eq!(after["scopes"], serde_json::json!(["observer", "requester"]));

    // The user grants Houndbit a folder in Protect, with a real file inside it.
    let folder = tempfile::tempdir().unwrap();
    std::fs::write(folder.path().join("notes.txt"), "the original text").unwrap();
    let g = admin.admin_grant_add(protect::APP_ID, folder.path().to_str().unwrap()).unwrap();
    let grant_id = g["grant_id"].as_str().unwrap().to_string();

    // Houndbit finds its own grant through the connector.
    let grants = protect::grants(home.path()).await.unwrap();
    assert_eq!(grants["grants"][0]["grant_id"].as_str(), Some(grant_id.as_str()));

    // Run the staged sandbox from Houndbit's side.
    let r = protect::sandbox_check(home.path(), &grant_id, "notes.txt").await.unwrap();
    assert_eq!(r["read_ok"], true);
    assert_eq!(r["staged_ok"], true);
    assert_eq!(r["wrote_original"], false);
    assert_eq!(r["originals_untouched"], true);
    let changes = r["diff"].as_array().unwrap();
    assert_eq!(changes[0]["path"], "notes.txt");
    assert_eq!(changes[0]["kind"], "modified");

    // The real file on disk is exactly as it was: Protect never wrote it.
    assert_eq!(std::fs::read_to_string(folder.path().join("notes.txt")).unwrap(), "the original text");

    server.abort();
    let _ = server.await;
    std::env::remove_var("HOUNDBIT_PROTECT_ENDPOINT");
}
