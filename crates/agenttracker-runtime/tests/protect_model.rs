//! The local model helper end to end, with a scripted model standing in for LM Studio/Ollama
//! (no real model runs in this test). The model lists, reads and stages; the real folder stays
//! the same until the user applies; a file changed by hand in the meantime is refused.
//! Its own test binary, because the hosted engine is process-wide.
use agenttracker_runtime::provider::{Provider, ScriptedProvider};
use agenttracker_runtime::{protect, protect_model};
use serde_json::{json, Value};
use std::sync::Arc;

fn endpoint(dir: &std::path::Path) -> String {
    if cfg!(windows) {
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        format!(r"\\.\pipe\houndbit-protect-model-{}-{n}", std::process::id())
    } else {
        dir.join("model.sock").to_string_lossy().into_owned()
    }
}

async fn wait_done(id: &str) -> Value {
    for _ in 0..500 {
        let v = protect_model::view(id).unwrap();
        if v["state"] != "working" && v["state"] != "stopping" {
            return v;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("the helper never finished: {}", protect_model::view(id).unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_local_model_works_through_protect_and_only_your_ok_writes_files() {
    let home = tempfile::tempdir().unwrap();
    let protect_dir = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    std::fs::write(folder.path().join("a.txt"), "hello\n").unwrap();
    std::fs::write(folder.path().join(".env"), "SECRET=1\n").unwrap();
    let ep = endpoint(protect_dir.path());
    std::env::set_var("HOUNDBIT_PROTECT_ENDPOINT", &ep);

    // Not hosted yet: the helper says so instead of starting.
    let p: Arc<dyn Provider> = Arc::new(ScriptedProvider::sequence("simulated", vec!["{}".into()]));
    assert!(protect_model::start(home.path().into(), p, "http://127.0.0.1:1234/v1", "m", "g", "f", "do it", 20_000).is_err());

    tokio::spawn(protect::host(protect_dir.path().join("records"), ep.clone()));
    for _ in 0..100 {
        if protect::status(home.path()).await["paired"] == true {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(protect::is_hosted());
    let g = protect::admin_add_folder(protect::APP_ID, &folder.path().to_string_lossy()).unwrap();
    let gid = g["grant_id"].as_str().unwrap().to_string();

    let replies = vec![
        json!({"tool": "list", "path": "", "text": "", "note": ""}).to_string(),
        // A chatty reply with the JSON inside still works.
        format!("Sure! {}", json!({"tool": "read", "path": "a.txt", "text": "", "note": ""})),
        json!({"tool": "read", "path": ".env", "text": "", "note": ""}).to_string(),
        json!({"tool": "write", "path": "a.txt", "text": "hello world\n", "note": ""}).to_string(),
        json!({"tool": "write", "path": "notes/new.txt", "text": "brand new\n", "note": ""}).to_string(),
        json!({"tool": "done", "path": "", "text": "", "note": "Added a word and a notes file."}).to_string(),
    ];
    let p: Arc<dyn Provider> = Arc::new(ScriptedProvider::sequence("simulated", replies));
    let job = protect_model::start(home.path().into(), p, "http://127.0.0.1:1234/v1", "scripted", &gid, &folder.path().to_string_lossy(), "Say hello world and add a notes file", 20_000).unwrap();
    let id = job["id"].as_str().unwrap().to_string();
    assert_eq!(job["local"], true);

    let v = wait_done(&id).await;
    assert_eq!(v["state"], "review", "{v}");
    assert_eq!(v["summary"], "Added a word and a notes file.");
    let steps = v["steps"].as_array().unwrap().iter().map(|s| s.as_str().unwrap().to_string()).collect::<Vec<_>>().join("\n");
    assert!(steps.contains("didn't allow reading .env"), "{steps}");
    assert!(steps.contains("suggested a change to a.txt"), "{steps}");

    // Nothing in the real folder changed yet.
    assert_eq!(std::fs::read_to_string(folder.path().join("a.txt")).unwrap(), "hello\n");
    assert!(!folder.path().join("notes/new.txt").exists());
    let changes = v["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 2, "{v}");
    let a = changes.iter().find(|c| c["path"] == "a.txt").unwrap();
    assert_eq!((a["kind"].as_str(), a["old_text"].as_str(), a["new_text"].as_str()), (Some("modified"), Some("hello\n"), Some("hello world\n")));

    // Apply just a.txt.
    let r = protect_model::apply(&id, Some(vec!["a.txt".into()])).unwrap();
    assert_eq!(r["results"][0]["applied"], true, "{r}");
    assert_eq!(std::fs::read_to_string(folder.path().join("a.txt")).unwrap(), "hello world\n");
    assert_eq!(r["state"], "review");

    // Someone makes notes/new.txt by hand; applying the model's version is refused.
    std::fs::create_dir_all(folder.path().join("notes")).unwrap();
    std::fs::write(folder.path().join("notes/new.txt"), "mine\n").unwrap();
    let r = protect_model::apply(&id, None).unwrap();
    assert_eq!(r["results"][0]["applied"], false, "{r}");
    assert_eq!(std::fs::read_to_string(folder.path().join("notes/new.txt")).unwrap(), "mine\n");

    // Throw the rest away; the job is finished and the real file is still the hand-made one.
    let r = protect_model::discard(&id, None).unwrap();
    assert_eq!(r["state"], "finished", "{r}");
    assert_eq!(r["changes"].as_array().unwrap().len(), 0);
    assert_eq!(std::fs::read_to_string(folder.path().join("notes/new.txt")).unwrap(), "mine\n");

    // A model that never replies in the right shape is stopped after a few tries.
    let p: Arc<dyn Provider> = Arc::new(ScriptedProvider::sequence("simulated", vec!["I can't do JSON, sorry.".into()]));
    let job = protect_model::start(home.path().into(), p, "http://127.0.0.1:1234/v1", "scripted", &gid, "", "anything", 20_000).unwrap();
    let v = wait_done(job["id"].as_str().unwrap()).await;
    assert_eq!(v["state"], "failed", "{v}");
    assert!(v["error"].as_str().unwrap().contains("expected format"));
}
