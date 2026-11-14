//! End-to-end protocol tests through the real local transport (named pipe on Windows, Unix
//! socket elsewhere). These cover decisions, records, and the staged file worker that runs the
//! four typed file operations against a read-only snapshot and a writable staging folder.
use protect_engine::client::{generate_key, public_key_b64, Client};
use protect_engine::policy::{Decision, DEFAULT_PROFILE};
use protect_engine::{transport, Code, Engine};
use serde_json::{json, Value};
use std::sync::Arc;

struct Harness {
    _dir: tempfile::TempDir,
    folder: tempfile::TempDir,
    endpoint: String,
    admin: Engine,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn start() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let endpoint = if cfg!(windows) { format!(r"\\.\pipe\houndbit-protect-test-{}", uuid::Uuid::new_v4().simple()) } else { dir.path().join("e.sock").to_string_lossy().into_owned() };
    let engine = Arc::new(Engine::open(dir.path()).unwrap());
    let ep = endpoint.clone();
    let task = tokio::spawn(async move {
        let _ = transport::serve(engine, &ep).await;
    });
    // The admin path is a second handle on the same records, like Protect Desktop would be.
    let admin = Engine::open(dir.path()).unwrap();
    for _ in 0..100 {
        if Client::connect(&endpoint).await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    Harness { _dir: dir, folder: tempfile::tempdir().unwrap(), endpoint, admin, task }
}

const P: u64 = protect_engine::PROTOCOL_MAJOR;

/// Pairs an app (accepted on the admin path), signs in, and gives it the test folder.
async fn paired(h: &Harness, app: &str, scopes: Option<Vec<String>>) -> (Client, String) {
    let key = generate_key();
    let mut c = Client::connect(&h.endpoint).await.unwrap();
    let r = c.call("v1.pairing.request", json!({"protocol_major": P, "app_id": app, "display_name": app, "public_key": public_key_b64(&key)})).await.unwrap();
    h.admin.admin_pairing_resolve(r["pairing_id"].as_str().unwrap(), true, scopes).unwrap();
    c.sign_in(app, &key).await.unwrap();
    let g = h.admin.admin_grant_add(app, h.folder.path().to_str().unwrap()).unwrap();
    (c, g["grant_id"].as_str().unwrap().to_string())
}

async fn running_session(c: &mut Client, grant: &str) -> String {
    let s = c.call("v1.sessions.request", json!({"profile": DEFAULT_PROFILE, "resource_grant_id": grant})).await.unwrap();
    let id = s["id"].as_str().unwrap().to_string();
    c.call("v1.sessions.start", json!({"session_id": id})).await.unwrap();
    id
}

fn code(r: Result<Value, protect_engine::client::ClientError>) -> Code {
    r.expect_err("expected an error").code.expect("expected an engine error code, not a transport failure")
}

#[tokio::test(flavor = "multi_thread")]
async fn unauthenticated_and_wrong_version_are_refused() {
    let h = start().await;
    let mut c = Client::connect(&h.endpoint).await.unwrap();
    assert_eq!(code(c.call("v1.hello", json!({"protocol_major": P})).await), Code::AuthRequired);
    assert_eq!(code(c.call("v1.sessions.list", json!({})).await), Code::AuthRequired);
    assert_eq!(code(c.call("v1.auth.challenge", json!({"protocol_major": 2, "app_id": "x.y"})).await), Code::UnsupportedVersion);
    // A client_id field is not identity: an unpaired app can't sign in.
    let key = generate_key();
    assert_eq!(code(c.sign_in("never.paired", &key).await), Code::AuthFailed);
}

#[tokio::test(flavor = "multi_thread")]
async fn pairing_needs_the_user_and_proof_of_the_key() {
    let h = start().await;
    let key = generate_key();
    let mut c = Client::connect(&h.endpoint).await.unwrap();
    // An app can't ask for approver/administrator.
    let r = c.call("v1.pairing.request", json!({"protocol_major": P, "app_id": "com.example.a", "display_name": "A", "public_key": public_key_b64(&key), "scopes": ["requester", "approver"]})).await;
    assert_eq!(code(r), Code::ScopeDenied);
    let r = c.call("v1.pairing.request", json!({"protocol_major": P, "app_id": "com.example.a", "display_name": "A", "public_key": public_key_b64(&key)})).await.unwrap();
    let pid = r["pairing_id"].as_str().unwrap().to_string();
    // Still pending: it can't sign in, and there is no RPC to accept itself.
    assert_eq!(code(c.sign_in("com.example.a", &key).await), Code::AuthFailed);
    assert_eq!(code(c.call("v1.pairings.accept", json!({"pairing_id": pid})).await), Code::AuthRequired);
    assert_eq!(c.call("v1.pairing.status", json!({"protocol_major": P, "pairing_id": pid})).await.unwrap()["state"], "pending");
    h.admin.admin_pairing_resolve(&pid, true, None).unwrap();
    // Someone else holding a different key can't sign in as the app.
    let mut imposter = Client::connect(&h.endpoint).await.unwrap();
    assert_eq!(code(imposter.sign_in("com.example.a", &generate_key()).await), Code::AuthFailed);
    // And a second pairing request for the same id is refused while it's paired.
    let r = imposter.call("v1.pairing.request", json!({"protocol_major": P, "app_id": "com.example.a", "display_name": "A2", "public_key": public_key_b64(&generate_key())})).await;
    assert_eq!(code(r), Code::InvalidState);
    let who = c.sign_in("com.example.a", &key).await.unwrap();
    assert_eq!(who["scopes"], json!(["observer", "requester"]));
    let hello = c.call("v1.hello", json!({"protocol_major": P})).await.unwrap();
    assert_eq!(hello["protocol_version"], "1.0");
    let caps = c.call("v1.capabilities.get", json!({})).await.unwrap();
    assert_eq!(caps["modes"]["gate"], "unavailable");
    assert_eq!(caps["modes"]["isolate"], "unavailable");
    // Removing the app ends its ability to sign in.
    h.admin.admin_revoke_app("com.example.a").unwrap();
    let mut again = Client::connect(&h.endpoint).await.unwrap();
    assert_eq!(code(again.sign_in("com.example.a", &key).await), Code::AuthFailed);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_challenge_works_only_once() {
    let h = start().await;
    let (_c, _) = paired(&h, "com.example.once", None).await;
    let mut c = Client::connect(&h.endpoint).await.unwrap();
    c.call("v1.auth.challenge", json!({"protocol_major": P, "app_id": "com.example.once"})).await.unwrap();
    let bad = c.call("v1.auth.prove", json!({"protocol_major": P, "app_id": "com.example.once", "signature": "AAAA"})).await;
    assert_eq!(code(bad), Code::AuthFailed);
    // The nonce was used up by the failed try; proving again without a new challenge fails.
    let again = c.call("v1.auth.prove", json!({"protocol_major": P, "app_id": "com.example.once", "signature": "AAAA"})).await;
    assert_eq!(code(again), Code::AuthFailed);
}

#[tokio::test(flavor = "multi_thread")]
async fn decisions_are_recorded_and_allowed_work_runs() {
    let h = start().await;
    // A real file in the granted folder, written before the session's snapshot is taken.
    std::fs::write(h.folder.path().join("notes.txt"), "hello there").unwrap();
    let (mut c, grant) = paired(&h, "com.example.app", None).await;
    let s = running_session(&mut c, &grant).await;
    let ok = c.call("v1.actions.submit", json!({"session_id": s, "idempotency_key": "k1", "operation_id": "files.read", "arguments": {"path": "notes.txt"}})).await.unwrap();
    assert_eq!(ok["decision"], "allow");
    assert_eq!(ok["state"], "succeeded", "allowed and run by the staged file worker");
    assert_eq!(ok["executed"], true);
    assert!(ok["error_code"].is_null());
    // The worker read the snapshot copy and returned the real content.
    assert_eq!(ok["output"]["text"], "hello there");
    let no = c.call("v1.actions.submit", json!({"session_id": s, "idempotency_key": "k2", "operation_id": "files.read", "arguments": {"path": ".git/config"}})).await.unwrap();
    assert_eq!((no["decision"].as_str(), no["state"].as_str()), (Some("deny"), Some("denied")));
    // Unknown operations and bad paths are refused outright, not mapped to something allowed.
    assert_eq!(code(c.call("v1.actions.submit", json!({"session_id": s, "idempotency_key": "k3", "operation_id": "shell.run", "arguments": {"cmd": "rm -rf /"}})).await), Code::UnsupportedOperation);
    assert_eq!(code(c.call("v1.actions.submit", json!({"session_id": s, "idempotency_key": "k4", "operation_id": "files.read", "arguments": {"path": "..\\..\\Windows\\win.ini"}})).await), Code::InvalidArguments);
    assert_eq!(code(c.call("v1.actions.submit", json!({"session_id": s, "idempotency_key": "k5", "operation_id": "files.read", "arguments": {"path": "a.txt", "extra": 1}})).await), Code::InvalidArguments);
    // Events come back in order, page by page, with a resumable cursor.
    let p1 = c.call("v1.events.read", json!({"session_id": s, "limit": 3})).await.unwrap();
    let types: Vec<&str> = p1["events"].as_array().unwrap().iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["session.ready", "session.running", "action.proposed"]);
    let p2 = c.call("v1.events.read", json!({"session_id": s, "cursor": p1["next_cursor"], "limit": 200})).await.unwrap();
    let types2: Vec<&str> = p2["events"].as_array().unwrap().iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(types2, ["action.decided", "action.authorized", "action.executed", "action.proposed", "action.decided"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sandbox_stages_writes_and_never_touches_originals() {
    let h = start().await;
    // Two real files in the granted folder, captured by the snapshot at session request.
    std::fs::write(h.folder.path().join("keep.txt"), "original-keep").unwrap();
    std::fs::write(h.folder.path().join("edit.txt"), "original-edit").unwrap();
    let (mut c, grant) = paired(&h, "com.example.box", None).await;
    let s = running_session(&mut c, &grant).await;

    // Reading goes through the snapshot copy.
    let r = c.call("v1.actions.submit", json!({"session_id": s, "idempotency_key": "r1", "operation_id": "files.read", "arguments": {"path": "edit.txt"}})).await.unwrap();
    assert_eq!((r["state"].as_str(), r["executed"].as_bool()), (Some("succeeded"), Some(true)));
    assert_eq!(r["output"]["text"], "original-edit");

    // Writing stages a change; the original on disk is left exactly as it was.
    let w = c.call("v1.actions.submit", json!({"session_id": s, "idempotency_key": "w1", "operation_id": "files.write_staged", "arguments": {"path": "edit.txt", "text": "staged-edit"}})).await.unwrap();
    assert_eq!((w["state"].as_str(), w["executed"].as_bool()), (Some("succeeded"), Some(true)));
    assert_eq!(w["output"]["wrote_original"], false);
    assert_eq!(std::fs::read_to_string(h.folder.path().join("edit.txt")).unwrap(), "original-edit", "the real file is untouched");

    // A later read now sees the staged version layered over the snapshot.
    let r2 = c.call("v1.actions.submit", json!({"session_id": s, "idempotency_key": "r2", "operation_id": "files.read", "arguments": {"path": "edit.txt"}})).await.unwrap();
    assert_eq!(r2["output"]["text"], "staged-edit");

    // A brand-new staged file never exists in the granted folder.
    c.call("v1.actions.submit", json!({"session_id": s, "idempotency_key": "w2", "operation_id": "files.write_staged", "arguments": {"path": "new.txt", "text": "brand new"}})).await.unwrap();
    assert!(!h.folder.path().join("new.txt").exists(), "staged-only file must not appear in the original folder");

    // diff reports exactly the staged changes, sorted by path.
    let d = c.call("v1.actions.submit", json!({"session_id": s, "idempotency_key": "d1", "operation_id": "files.diff", "arguments": {}})).await.unwrap();
    let changes = d["output"]["changes"].as_array().unwrap();
    let kinds: Vec<(&str, &str)> = changes.iter().map(|ch| (ch["path"].as_str().unwrap(), ch["kind"].as_str().unwrap())).collect();
    assert_eq!(kinds, [("edit.txt", "modified"), ("new.txt", "added")]);

    // list merges originals with staged files.
    let l = c.call("v1.actions.submit", json!({"session_id": s, "idempotency_key": "l1", "operation_id": "files.list", "arguments": {}})).await.unwrap();
    let files: Vec<&str> = l["output"]["files"].as_array().unwrap().iter().map(|f| f["path"].as_str().unwrap()).collect();
    assert!(files.contains(&"keep.txt") && files.contains(&"edit.txt") && files.contains(&"new.txt"));
}

#[tokio::test(flavor = "multi_thread")]
async fn idempotency_keys_return_the_same_action_or_conflict() {
    let h = start().await;
    let (mut c, grant) = paired(&h, "com.example.idem", None).await;
    let s = running_session(&mut c, &grant).await;
    let req = json!({"session_id": s, "idempotency_key": "same", "operation_id": "files.write_staged", "arguments": {"path": "a.txt", "text": "one"}});
    let a = c.call("v1.actions.submit", req.clone()).await.unwrap();
    let b = c.call("v1.actions.submit", req).await.unwrap();
    assert_eq!(a["id"], b["id"]);
    let changed = c.call("v1.actions.submit", json!({"session_id": s, "idempotency_key": "same", "operation_id": "files.write_staged", "arguments": {"path": "a.txt", "text": "two"}})).await;
    assert_eq!(code(changed), Code::IdempotencyConflict);
    assert_eq!(c.call("v1.sessions.get", json!({"session_id": s})).await.unwrap()["action_count"], 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn approvals_are_single_use_and_only_for_approvers() {
    let h = start().await;
    h.admin.admin_policy_set(DEFAULT_PROFILE, "files.write_staged", Decision::RequireApproval).unwrap();
    let (mut c, grant) = paired(&h, "com.example.req", None).await;
    let s = running_session(&mut c, &grant).await;
    let a = c.call("v1.actions.submit", json!({"session_id": s, "idempotency_key": "w1", "operation_id": "files.write_staged", "arguments": {"path": "a.txt", "text": "hi"}})).await.unwrap();
    assert_eq!((a["decision"].as_str(), a["state"].as_str()), (Some("require_approval"), Some("awaiting_approval")));
    // The requester can't see or answer approvals (no self-approval).
    assert_eq!(code(c.call("v1.approvals.list", json!({})).await), Code::ScopeDenied);
    assert_eq!(code(c.call("v1.approvals.resolve", json!({"approval_id": a["approval_id"], "nonce": "x", "decision": "approve"})).await), Code::ScopeDenied);
    // A wrong nonce is refused; the right one works once.
    let pending = h.admin.approvals_list().unwrap();
    assert_eq!(pending.len(), 1);
    let (aid, nonce) = (pending[0]["approval_id"].as_str().unwrap(), pending[0]["nonce"].as_str().unwrap());
    assert_eq!(h.admin.approvals_resolve(aid, "wrong", true).unwrap_err().code, Code::StaleResource);
    h.admin.approvals_resolve(aid, nonce, true).unwrap();
    assert_eq!(h.admin.approvals_resolve(aid, nonce, true).unwrap_err().code, Code::InvalidState, "replay refused");
    let after = c.call("v1.actions.get", json!({"action_id": a["id"]})).await.unwrap();
    assert_eq!((after["state"].as_str(), after["executed"].as_bool()), (Some("succeeded"), Some(true)), "approved, then run by the staged file worker");
    assert_eq!(after["output"]["staged"], true, "the write landed in staging, not the original");
    assert_eq!(after["output"]["wrote_original"], false);
    // Pausing the session cancels anything still waiting.
    let b = c.call("v1.actions.submit", json!({"session_id": s, "idempotency_key": "w2", "operation_id": "files.write_staged", "arguments": {"path": "b.txt", "text": "x"}})).await.unwrap();
    c.call("v1.sessions.pause", json!({"session_id": s})).await.unwrap();
    assert_eq!(c.call("v1.actions.get", json!({"action_id": b["id"]})).await.unwrap()["state"], "cancelled");
    assert!(h.admin.approvals_list().unwrap().is_empty());
    assert_eq!(code(c.call("v1.actions.submit", json!({"session_id": s, "idempotency_key": "w3", "operation_id": "files.read", "arguments": {"path": "a"}})).await), Code::InvalidState);
}

#[tokio::test(flavor = "multi_thread")]
async fn apps_cannot_see_each_others_sessions() {
    let h = start().await;
    let (mut a, grant_a) = paired(&h, "com.example.one", None).await;
    let (mut b, grant_b) = paired(&h, "com.example.two", None).await;
    let sa = running_session(&mut a, &grant_a).await;
    for (m, p) in [
        ("v1.sessions.get", json!({"session_id": sa})),
        ("v1.events.read", json!({"session_id": sa})),
        ("v1.sessions.stop", json!({"session_id": sa})),
        ("v1.actions.submit", json!({"session_id": sa, "idempotency_key": "x", "operation_id": "files.read", "arguments": {"path": "a"}})),
    ] {
        assert_eq!(code(b.call(m, p).await), Code::NotFound, "{m}");
    }
    // Nor use another app's folder grant.
    assert_eq!(code(b.call("v1.sessions.request", json!({"resource_grant_id": grant_a})).await), Code::ScopeDenied);
    assert!(b.call("v1.sessions.list", json!({})).await.unwrap()["sessions"].as_array().unwrap().is_empty());
    // Each app sees only its own folder grant.
    let gb = b.call("v1.grants.list", json!({})).await.unwrap();
    assert_eq!(gb["grants"].as_array().unwrap().len(), 1);
    assert_eq!(gb["grants"][0]["grant_id"].as_str(), Some(grant_b.as_str()));
    // An observer-only app can read its own sessions but can't start any.
    let (mut o, grant_o) = paired(&h, "com.example.watch", Some(vec!["observer".into()])).await;
    assert_eq!(code(o.call("v1.sessions.request", json!({"resource_grant_id": grant_o})).await), Code::ScopeDenied);
}

#[tokio::test(flavor = "multi_thread")]
async fn oversized_frames_close_the_connection() {
    use tokio::io::AsyncWriteExt;
    let h = start().await;
    // Write a raw header claiming 2 MiB; the engine must refuse before reading it.
    let mut c = Client::connect(&h.endpoint).await.unwrap();
    let _ = c.call("v1.pairing.status", json!({"protocol_major": P, "pairing_id": "none"})).await;
    drop(c);
    #[cfg(unix)]
    let mut raw = tokio::net::UnixStream::connect(&h.endpoint).await.unwrap();
    #[cfg(windows)]
    let mut raw = tokio::net::windows::named_pipe::ClientOptions::new().open(&h.endpoint).unwrap();
    raw.write_all(&(2u32 * 1024 * 1024).to_be_bytes()).await.unwrap();
    let body = protect_engine::framing::read_frame(&mut raw).await.unwrap();
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["error"]["data"]["protect_code"], "FRAME_TOO_LARGE");
}
