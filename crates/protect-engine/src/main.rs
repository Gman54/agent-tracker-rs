//! protect-engine: runs the engine, plus the local admin commands that stand in for the
//! Protect Desktop window until it exists.
use protect_engine::policy::{Decision, DEFAULT_PROFILE};
use protect_engine::{paths, transport, Engine};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

const HELP: &str = "Houndbit Protect engine (decides, records, and runs allowed file actions in a staged sandbox)

Usage:
  protect-engine serve
  protect-engine pairings                      list pairing requests and paired apps
  protect-engine pairings accept <pairing-id> [--scopes requester,observer]
  protect-engine pairings reject <pairing-id>
  protect-engine pairings remove <app-id>
  protect-engine folders                       list folder grants
  protect-engine folders add <app-id> <folder>
  protect-engine approvals                     list actions waiting for you
  protect-engine approvals approve <approval-id>
  protect-engine approvals decline <approval-id>
  protect-engine policy                        show the current rules
  protect-engine policy set <operation> allow|ask|deny

Options (any command): --data-dir <folder>  --endpoint <pipe name>";

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let take_opt = |args: &mut Vec<String>, name: &str| -> Option<String> {
        let i = args.iter().position(|a| a == name)?;
        args.remove(i);
        (i < args.len()).then(|| args.remove(i))
    };
    let data_dir = take_opt(&mut args, "--data-dir").map(PathBuf::from).unwrap_or_else(paths::default_data_dir);
    let endpoint = take_opt(&mut args, "--endpoint").unwrap_or_else(|| paths::default_endpoint(&data_dir));
    let scopes = take_opt(&mut args, "--scopes");
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    if a.is_empty() || matches!(a[0], "-h" | "--help" | "help") {
        println!("{HELP}");
        return;
    }
    let engine = match Engine::open(&data_dir) {
        Ok(e) => e,
        Err(e) => fail(&format!("Couldn't open the engine's records in {}: {e}", data_dir.display())),
    };
    let out: Result<Value, protect_engine::ProtectError> = match a.as_slice() {
        ["serve"] => {
            println!("Houndbit Protect engine {} is listening on {endpoint}", protect_engine::ENGINE_VERSION);
            println!("Records: {}", data_dir.display());
            println!("Note: allowed file actions run in a staged sandbox (snapshot in, staging out); your originals are never written.");
            let rt = tokio::runtime::Runtime::new().unwrap_or_else(|e| fail(&e.to_string()));
            if let Err(e) = rt.block_on(transport::serve(Arc::new(engine), &endpoint)) {
                fail(&format!("The engine stopped: {e}"));
            }
            return;
        }
        ["pairings"] => engine.admin_pairings().and_then(|p| Ok(serde_json::json!({"requests": p, "paired_apps": engine.admin_apps()?}))),
        ["pairings", "accept", id] => engine.admin_pairing_resolve(id, true, scopes.map(|s| s.split(',').map(|x| x.trim().to_string()).collect())),
        ["pairings", "reject", id] => engine.admin_pairing_resolve(id, false, None),
        ["pairings", "remove", app] => engine.admin_revoke_app(app),
        ["folders"] => engine.admin_grants().map(Value::from),
        ["folders", "add", app, folder] => engine.admin_grant_add(app, folder),
        ["approvals"] => engine.approvals_list().map(Value::from),
        ["approvals", verb @ ("approve" | "decline"), id] => {
            let nonce = engine.approvals_list().ok().and_then(|l| l.into_iter().find(|x| x["approval_id"] == *id)).and_then(|x| x["nonce"].as_str().map(str::to_string));
            match nonce {
                Some(n) => engine.approvals_resolve(id, &n, *verb == "approve"),
                None => fail("No waiting approval with that id (it may have expired or been handled)."),
            }
        }
        ["policy"] => engine.current_policy(DEFAULT_PROFILE).map(|(rev, p)| serde_json::json!({"revision": rev, "policy": p})),
        ["policy", "set", op, d] => {
            let dec = match *d {
                "allow" => Decision::Allow,
                "ask" => Decision::RequireApproval,
                "deny" => Decision::Deny,
                _ => fail("Use allow, ask or deny."),
            };
            engine.admin_policy_set(DEFAULT_PROFILE, op, dec).map(|rev| serde_json::json!({"new_revision": rev}))
        }
        _ => fail(&format!("Unknown command.\n\n{HELP}")),
    };
    match out {
        Ok(v) => println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default()),
        Err(e) => fail(&format!("{} ({})", e.message, e.code.as_str())),
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(1)
}
