//! Protect running inside Houndbit: Houndbit starts the engine itself, links itself to it with no
//! button and no question,
//! adds a folder from its own window, and runs the staged sandbox without touching the original.
//! Its own test binary (own process), because the hosted engine is process-wide.
use agenttracker_runtime::protect;

fn endpoint(dir: &std::path::Path) -> String {
    if cfg!(windows) {
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        format!(r"\\.\pipe\houndbit-protect-hosted-{}-{n}", std::process::id())
    } else {
        dir.join("hosted.sock").to_string_lossy().into_owned()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn houndbit_hosts_protect_and_runs_the_sandbox() {
    let home = tempfile::tempdir().unwrap();
    let protect_dir = tempfile::tempdir().unwrap();
    let ep = endpoint(protect_dir.path());
    std::env::set_var("HOUNDBIT_PROTECT_ENDPOINT", &ep);

    // Before Houndbit hosts anything, the controls say so instead of failing oddly.
    assert_eq!(protect::admin_overview()["hosted"], false);
    assert!(protect::admin_add_folder(protect::APP_ID, ".").is_err());

    let server = tokio::spawn(protect::host(protect_dir.path().join("records"), ep.clone()));
    let mut up = false;
    for _ in 0..100 {
        if protect::status(home.path()).await["running"] == true {
            up = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(up, "hosted Protect never came up on {ep}");
    assert!(protect::is_hosted());

    // Asking for the status was enough: Houndbit linked itself, with nothing waiting for an OK.
    let s = protect::status(home.path()).await;
    assert_eq!(s["paired"], true, "{s}");
    assert_eq!(s["hosted"], true);
    assert_eq!(s["message"], "Houndbit is linked to Protect automatically.");
    assert_eq!(protect::admin_overview()["pairings"].as_array().unwrap().len(), 0);
    // Calling it again doesn't pair twice.
    protect::ensure_linked(home.path()).await.unwrap();
    assert_eq!(protect::admin_overview()["apps"].as_array().unwrap().iter().filter(|a| a["app_id"] == protect::APP_ID).count(), 1);

    // Another app asks to connect; it waits for the user's OK in Houndbit's window.
    let other = protect_engine::client::load_or_create_key(&home.path().join("other.key")).unwrap();
    let mut oc = protect_engine::client::Client::connect(&ep).await.unwrap();
    let req = oc
        .call(
            "v1.pairing.request",
            serde_json::json!({"protocol_major": protect_engine::PROTOCOL_MAJOR, "app_id": "com.example.other", "display_name": "Other",
                "public_key": protect_engine::client::public_key_b64(&other), "scopes": ["requester"]}),
        )
        .await
        .unwrap();
    let ov = protect::admin_overview();
    assert_eq!(ov["hosted"], true);
    assert_eq!(ov["pairings"].as_array().unwrap().len(), 1);
    assert!(ov["approvals"].as_array().unwrap().iter().all(|a| a.get("nonce").is_none()));
    protect::admin_pairing(req["pairing_id"].as_str().unwrap(), false).unwrap();
    assert_eq!(protect::admin_overview()["pairings"].as_array().unwrap().len(), 0);

    // Add a folder from Houndbit's window, then try the sandbox on a real file.
    let folder = tempfile::tempdir().unwrap();
    std::fs::write(folder.path().join("notes.txt"), "the original text").unwrap();
    assert!(protect::admin_add_folder(protect::APP_ID, "  ").is_err());
    let g = protect::admin_add_folder(protect::APP_ID, folder.path().to_str().unwrap()).unwrap();
    let grant_id = g["grant_id"].as_str().unwrap().to_string();
    let r = protect::sandbox_check(home.path(), &grant_id, "notes.txt").await.unwrap();
    assert_eq!(r["staged_ok"], true);
    assert_eq!(r["originals_untouched"], true);
    assert_eq!(std::fs::read_to_string(folder.path().join("notes.txt")).unwrap(), "the original text");

    // If Houndbit's entry is removed, it links itself again on the next look, but the folders
    // given before are gone (removing an app takes its folders away).
    protect::admin_remove_app(protect::APP_ID).unwrap();
    assert_eq!(protect::status(home.path()).await["paired"], true);
    assert!(protect::grants(home.path()).await.unwrap()["grants"].as_array().unwrap().is_empty());

    // Houndbit's key was replaced (e.g. its data folder was reset): it replaces its own old entry.
    let home2 = tempfile::tempdir().unwrap();
    let s2 = protect::status(home2.path()).await;
    assert_eq!(s2["paired"], true, "{s2}");

    server.abort();
    let _ = server.await;
    std::env::remove_var("HOUNDBIT_PROTECT_ENDPOINT");
}
