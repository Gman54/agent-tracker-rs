//! Engine logic on top of its own database. No in-memory authority: every decision is written in
//! the same transaction that creates the action, so a crash can't leave an unrecorded "allow".
use crate::errors::{Code, ProtectError, Result};
use crate::policy::{self, Decision, Policy};
use crate::{now_ms, store};
use base64::Engine as _;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Mutex;

/// Scopes an app may ask for itself. approver/administrator are only ever given by the user
/// through the admin path, never through a pairing request.
pub const REQUESTABLE_SCOPES: &[&str] = &["observer", "requester"];
pub const ALL_SCOPES: &[&str] = &["observer", "requester", "approver", "administrator"];
pub const PAIRING_RATE: (i64, i64) = (5, 10 * 60 * 1000);
pub const AUTH_FAIL_RATE: (i64, i64) = (5, 5 * 60 * 1000);
/// Shown on every action that would have run: nothing executes in Milestone A.
pub const NOT_RUN: &str = "Nothing was run: this build has no execution backend yet (Milestone A).";

pub struct Engine {
    db: Mutex<Connection>,
    pub engine_id: String,
}

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

fn err(code: Code, msg: impl Into<String>) -> ProtectError {
    ProtectError::new(code, msg)
}

pub fn auth_message(engine_id: &str, nonce: &str) -> Vec<u8> {
    format!("houndbit-protect-auth-v1\n{engine_id}\n{nonce}").into_bytes()
}

fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}

fn event(c: &Connection, session: Option<&str>, action: Option<&str>, ty: &str, source: &str, rev: Option<i64>, payload: Value) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO events(event_id, session_id, action_id, ts, type, source, policy_revision, payload) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        params![new_id("ev"), session, action, now_ms(), ty, source, rev, payload.to_string()],
    )?;
    Ok(())
}

impl Engine {
    pub fn open(dir: &Path) -> Result<Self> {
        let c = store::open(dir)?;
        let engine_id = match c.query_row("SELECT value FROM meta WHERE key='engine_id'", [], |r| r.get::<_, String>(0)).optional()? {
            Some(id) => id,
            None => {
                let id = new_id("eng");
                c.execute("INSERT INTO meta(key, value) VALUES ('engine_id', ?1)", [&id])?;
                id
            }
        };
        let has_policy: bool = c.query_row("SELECT COUNT(*) FROM policy_revisions WHERE profile=?1", [policy::DEFAULT_PROFILE], |r| r.get::<_, i64>(0))? > 0;
        if !has_policy {
            let p = Policy::staged_project_v1();
            c.execute(
                "INSERT INTO policy_revisions(profile, revision, body, hash, created_at) VALUES (?1, 1, ?2, ?3, ?4)",
                params![p.profile, serde_json::to_string(&p).unwrap_or_default(), p.hash(), now_ms()],
            )?;
        }
        Ok(Self { db: Mutex::new(c), engine_id })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|p| p.into_inner())
    }

    // ---------------------------------------------------------------- pairing and identity

    pub fn pairing_request(&self, app_id: &str, display_name: &str, public_key: &str, scopes: &[String]) -> Result<Value> {
        let ok_id = (3..=100).contains(&app_id.len()) && app_id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | '_'));
        if !ok_id {
            return Err(err(Code::InvalidArguments, "app_id must be 3-100 characters of a-z, 0-9, '.', '-' or '_'"));
        }
        let name = display_name.trim();
        if name.is_empty() || name.chars().count() > 80 {
            return Err(err(Code::InvalidArguments, "display_name must be 1-80 characters"));
        }
        decode_key(public_key)?;
        if let Some(s) = scopes.iter().find(|s| !REQUESTABLE_SCOPES.contains(&s.as_str())) {
            return Err(err(Code::ScopeDenied, format!("An app can't ask for the \"{s}\" scope; only the user can give it in Protect.")));
        }
        let c = self.conn();
        let recent: i64 = c.query_row("SELECT COUNT(*) FROM pairing_requests WHERE created_at > ?1", [now_ms() - PAIRING_RATE.1], |r| r.get(0))?;
        if recent >= PAIRING_RATE.0 {
            return Err(err(Code::RateLimited, "Too many pairing requests recently. Try again in a few minutes."));
        }
        let paired: bool = c.query_row("SELECT COUNT(*) FROM apps WHERE app_id=?1 AND revoked_at IS NULL", [app_id], |r| r.get::<_, i64>(0))? > 0;
        if paired {
            return Err(err(Code::InvalidState, "This app is already paired. Remove it in Protect first if you want to pair it again."));
        }
        let id = new_id("pair");
        let scopes = if scopes.is_empty() { REQUESTABLE_SCOPES.iter().map(|s| s.to_string()).collect() } else { scopes.to_vec() };
        c.execute(
            "INSERT INTO pairing_requests(id, app_id, display_name, public_key, requested_scopes, state, created_at) VALUES (?1,?2,?3,?4,?5,'pending',?6)",
            params![id, app_id, name, public_key, scopes.join(","), now_ms()],
        )?;
        event(&c, None, None, "pairing.requested", "engine", None, json!({"pairing_id": id, "app_id": app_id, "display_name": name, "scopes": scopes}))?;
        Ok(json!({"pairing_id": id, "state": "pending", "message": "Open Houndbit Protect to accept or reject this app."}))
    }

    pub fn pairing_status(&self, pairing_id: &str) -> Result<Value> {
        let c = self.conn();
        let st: Option<String> = c.query_row("SELECT state FROM pairing_requests WHERE id=?1", [pairing_id], |r| r.get(0)).optional()?;
        let st = st.ok_or_else(|| err(Code::NotFound, "No such pairing request"))?;
        Ok(json!({"pairing_id": pairing_id, "state": st}))
    }

    pub fn admin_pairings(&self) -> Result<Vec<Value>> {
        let c = self.conn();
        let mut s = c.prepare("SELECT id, app_id, display_name, requested_scopes, state, created_at FROM pairing_requests ORDER BY created_at DESC LIMIT 100")?;
        let rows = s.query_map([], |r| {
            Ok(json!({"pairing_id": r.get::<_, String>(0)?, "app_id": r.get::<_, String>(1)?, "display_name": r.get::<_, String>(2)?,
                "requested_scopes": r.get::<_, String>(3)?, "state": r.get::<_, String>(4)?, "created_at": r.get::<_, i64>(5)?}))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Admin path only (Protect's own control surface). `scopes` None = what the app asked for.
    pub fn admin_pairing_resolve(&self, pairing_id: &str, accept: bool, scopes: Option<Vec<String>>) -> Result<Value> {
        let mut c = self.conn();
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(String, String, String, String, String)> = tx
            .query_row("SELECT app_id, display_name, public_key, requested_scopes, state FROM pairing_requests WHERE id=?1", [pairing_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .optional()?;
        let (app_id, name, key, requested, state) = row.ok_or_else(|| err(Code::NotFound, "No such pairing request"))?;
        if state != "pending" {
            return Err(err(Code::InvalidState, format!("This request was already {state}.")));
        }
        if accept {
            let scopes = scopes.unwrap_or_else(|| requested.split(',').map(str::to_string).collect());
            if let Some(s) = scopes.iter().find(|s| !ALL_SCOPES.contains(&s.as_str())) {
                return Err(err(Code::InvalidArguments, format!("unknown scope \"{s}\"")));
            }
            tx.execute("DELETE FROM apps WHERE app_id=?1 AND revoked_at IS NOT NULL", [&app_id])?;
            tx.execute("INSERT INTO apps(app_id, display_name, public_key, scopes, paired_at) VALUES (?1,?2,?3,?4,?5)", params![app_id, name, key, scopes.join(","), now_ms()])?;
        }
        let new_state = if accept { "accepted" } else { "rejected" };
        tx.execute("UPDATE pairing_requests SET state=?1, resolved_at=?2 WHERE id=?3", params![new_state, now_ms(), pairing_id])?;
        event(&tx, None, None, &format!("pairing.{new_state}"), "user", None, json!({"pairing_id": pairing_id, "app_id": app_id}))?;
        tx.commit()?;
        Ok(json!({"pairing_id": pairing_id, "state": new_state, "app_id": app_id}))
    }

    pub fn admin_revoke_app(&self, app_id: &str) -> Result<Value> {
        let c = self.conn();
        let n = c.execute("UPDATE apps SET revoked_at=?1 WHERE app_id=?2 AND revoked_at IS NULL", params![now_ms(), app_id])?;
        if n == 0 {
            return Err(err(Code::NotFound, "No paired app with that id"));
        }
        c.execute("UPDATE resource_grants SET revoked_at=?1 WHERE app_id=?2 AND revoked_at IS NULL", params![now_ms(), app_id])?;
        event(&c, None, None, "pairing.revoked", "user", None, json!({"app_id": app_id}))?;
        Ok(json!({"app_id": app_id, "revoked": true}))
    }

    pub fn admin_apps(&self) -> Result<Vec<Value>> {
        let c = self.conn();
        let mut s = c.prepare("SELECT app_id, display_name, scopes, paired_at, revoked_at FROM apps ORDER BY paired_at")?;
        let rows = s.query_map([], |r| {
            Ok(json!({"app_id": r.get::<_, String>(0)?, "display_name": r.get::<_, String>(1)?, "scopes": r.get::<_, String>(2)?,
                "paired_at": r.get::<_, i64>(3)?, "revoked_at": r.get::<_, Option<i64>>(4)?}))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Checks a challenge proof. Returns the app's granted scopes.
    pub fn verify_proof(&self, app_id: &str, nonce: &str, signature_b64: &str) -> Result<Vec<String>> {
        let c = self.conn();
        let fails: i64 = c.query_row("SELECT COUNT(*) FROM auth_failures WHERE app_id=?1 AND ts > ?2", params![app_id, now_ms() - AUTH_FAIL_RATE.1], |r| r.get(0))?;
        if fails >= AUTH_FAIL_RATE.0 {
            return Err(err(Code::RateLimited, "Too many failed sign-ins for this app. Wait a few minutes."));
        }
        let row: Option<(String, String)> = c.query_row("SELECT public_key, scopes FROM apps WHERE app_id=?1 AND revoked_at IS NULL", [app_id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let fail = |c: &Connection| -> Result<Vec<String>> {
            c.execute("INSERT INTO auth_failures(app_id, ts) VALUES (?1, ?2)", params![app_id, now_ms()])?;
            Err(err(Code::AuthFailed, "This app isn't paired, or its proof didn't match."))
        };
        let Some((key, scopes)) = row else { return fail(&c) };
        let ok = (|| {
            let vk = decode_key(&key).ok()?;
            let sig_bytes: [u8; 64] = b64().decode(signature_b64).ok()?.try_into().ok()?;
            vk.verify(&auth_message(&self.engine_id, nonce), &Signature::from_bytes(&sig_bytes)).ok()
        })();
        if ok.is_none() {
            return fail(&c);
        }
        Ok(scopes.split(',').filter(|s| !s.is_empty()).map(str::to_string).collect())
    }

    // ---------------------------------------------------------------- grants and policy

    pub fn admin_grant_add(&self, app_id: &str, folder: &str) -> Result<Value> {
        let c = self.conn();
        let paired: bool = c.query_row("SELECT COUNT(*) FROM apps WHERE app_id=?1 AND revoked_at IS NULL", [app_id], |r| r.get::<_, i64>(0))? > 0;
        if !paired {
            return Err(err(Code::NotFound, "Pair the app before giving it a folder"));
        }
        let canon = std::fs::canonicalize(folder).map_err(|e| err(Code::StaleResource, format!("Folder can't be opened: {e}")))?;
        if !canon.is_dir() {
            return Err(err(Code::InvalidArguments, "That isn't a folder"));
        }
        let id = new_id("grant");
        let f = canon.to_string_lossy().into_owned();
        c.execute("INSERT INTO resource_grants(id, app_id, folder, created_at) VALUES (?1,?2,?3,?4)", params![id, app_id, f, now_ms()])?;
        event(&c, None, None, "grant.added", "user", None, json!({"grant_id": id, "app_id": app_id, "folder": f}))?;
        Ok(json!({"grant_id": id, "app_id": app_id, "folder": f}))
    }

    pub fn admin_grants(&self) -> Result<Vec<Value>> {
        let c = self.conn();
        let mut s = c.prepare("SELECT id, app_id, folder, revoked_at FROM resource_grants ORDER BY created_at")?;
        let rows = s.query_map([], |r| Ok(json!({"grant_id": r.get::<_, String>(0)?, "app_id": r.get::<_, String>(1)?, "folder": r.get::<_, String>(2)?, "revoked_at": r.get::<_, Option<i64>>(3)?})))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The calling app's own active folder grants, so it can start a session without anyone
    /// copying an id across by hand. Other apps' grants are never listed.
    pub fn grants_for_app(&self, app_id: &str) -> Result<Vec<Value>> {
        let c = self.conn();
        let mut s = c.prepare("SELECT id, folder FROM resource_grants WHERE app_id=?1 AND revoked_at IS NULL ORDER BY created_at")?;
        let rows = s.query_map([app_id], |r| Ok(json!({"grant_id": r.get::<_, String>(0)?, "folder": r.get::<_, String>(1)?})))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn current_policy(&self, profile: &str) -> Result<(i64, Policy)> {
        let c = self.conn();
        current_policy(&c, profile)
    }

    /// Creates a new policy revision with one operation's decision changed.
    pub fn admin_policy_set(&self, profile: &str, op: &str, decision: Decision) -> Result<i64> {
        if !policy::OPERATIONS.contains(&op) {
            return Err(err(Code::UnsupportedOperation, format!("\"{op}\" isn't a registered operation")));
        }
        let mut c = self.conn();
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (rev, mut p) = current_policy(&tx, profile)?;
        p.operations.insert(op.into(), decision);
        let new_rev = rev + 1;
        tx.execute("INSERT INTO policy_revisions(profile, revision, body, hash, created_at) VALUES (?1,?2,?3,?4,?5)", params![profile, new_rev, serde_json::to_string(&p).unwrap_or_default(), p.hash(), now_ms()])?;
        event(&tx, None, None, "policy.revised", "user", Some(new_rev), json!({"profile": profile, "operation": op, "decision": decision.as_str()}))?;
        tx.commit()?;
        Ok(new_rev)
    }

    // ---------------------------------------------------------------- sessions

    pub fn sessions_request(&self, app_id: &str, profile: &str, grant_id: &str) -> Result<Value> {
        let c = self.conn();
        let (rev, _) = current_policy(&c, profile)?;
        let folder: Option<String> = c.query_row("SELECT folder FROM resource_grants WHERE id=?1 AND app_id=?2 AND revoked_at IS NULL", params![grant_id, app_id], |r| r.get(0)).optional()?;
        let folder = folder.ok_or_else(|| err(Code::ScopeDenied, "That folder grant doesn't exist or wasn't given to this app."))?;
        if !Path::new(&folder).is_dir() {
            return Err(err(Code::StaleResource, "The granted folder no longer exists."));
        }
        let id = new_id("ses");
        let now = now_ms();
        c.execute(
            "INSERT INTO sessions(id, app_id, profile, policy_revision, grant_id, state, created_at, updated_at) VALUES (?1,?2,?3,?4,?5,'ready',?6,?6)",
            params![id, app_id, profile, rev, grant_id, now],
        )?;
        event(&c, Some(&id), None, "session.ready", "engine", Some(rev), json!({"profile": profile, "grant_id": grant_id}))?;
        drop(c);
        self.sessions_get(app_id, &id)
    }

    pub fn sessions_get(&self, app_id: &str, id: &str) -> Result<Value> {
        let c = self.conn();
        session_view(&c, app_id, id)
    }

    pub fn sessions_list(&self, app_id: &str) -> Result<Vec<Value>> {
        let c = self.conn();
        let ids: Vec<String> = c.prepare("SELECT id FROM sessions WHERE app_id=?1 ORDER BY created_at DESC LIMIT 100")?.query_map([app_id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        ids.iter().map(|id| session_view(&c, app_id, id)).collect()
    }

    pub fn sessions_transition(&self, app_id: &str, id: &str, to: &str) -> Result<Value> {
        let mut c = self.conn();
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state = owned_session_state(&tx, app_id, id)?;
        let ok = matches!((state.as_str(), to), ("ready" | "paused", "running") | ("running", "paused") | ("ready" | "running" | "paused", "completed"));
        if !ok {
            return Err(err(Code::InvalidState, format!("A {state} session can't become {to}.")));
        }
        let now = now_ms();
        tx.execute("UPDATE sessions SET state=?1, updated_at=?2, started_at=COALESCE(started_at, CASE WHEN ?1='running' THEN ?2 END) WHERE id=?3", params![to, now, id])?;
        if to != "running" {
            // Pausing or stopping revokes future permits: nothing waiting can go through.
            tx.execute("UPDATE approvals SET state='cancelled', resolved_at=?1 WHERE session_id=?2 AND state='pending'", params![now, id])?;
            tx.execute("UPDATE actions SET state='cancelled', updated_at=?1 WHERE session_id=?2 AND state='awaiting_approval'", params![now, id])?;
        }
        event(&tx, Some(id), None, &format!("session.{to}"), "engine", None, json!({"from": state}))?;
        tx.commit()?;
        session_view(&c, app_id, id)
    }

    // ---------------------------------------------------------------- actions

    pub fn actions_submit(&self, app_id: &str, session_id: &str, key: &str, op: &str, args: &Value) -> Result<Value> {
        if key.is_empty() || key.len() > 200 {
            return Err(err(Code::InvalidArguments, "idempotency_key must be 1-200 characters"));
        }
        let args_hash = policy::hash_value(&json!({"operation": op, "arguments": args}));
        let mut c = self.conn();
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state = owned_session_state(&tx, app_id, session_id)?;
        let existing: Option<(String, String)> = tx.query_row("SELECT id, args_hash FROM actions WHERE session_id=?1 AND idempotency_key=?2", params![session_id, key], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        if let Some((id, h)) = existing {
            drop(tx);
            if h != args_hash {
                return Err(err(Code::IdempotencyConflict, "This idempotency key was already used for a different request."));
            }
            return action_view(&c, app_id, &id);
        }
        if state != "running" {
            return Err(err(Code::InvalidState, format!("The session is {state}; start it before submitting actions.")));
        }
        let (rev, profile, started, count): (i64, String, Option<i64>, i64) =
            tx.query_row("SELECT policy_revision, profile, started_at, action_count FROM sessions WHERE id=?1", [session_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        let p = policy_at(&tx, &profile, rev)?;
        let now = now_ms();
        if started.map(|s| now - s > p.max_session_minutes as i64 * 60_000).unwrap_or(false) {
            tx.execute("UPDATE sessions SET state='completed', updated_at=?1 WHERE id=?2", params![now, session_id])?;
            event(&tx, Some(session_id), None, "session.completed", "engine", Some(rev), json!({"reason": "time limit reached"}))?;
            tx.commit()?;
            return Err(err(Code::PolicyDenied, format!("The session reached its {}-minute limit and was stopped.", p.max_session_minutes)));
        }
        if count as u64 >= p.max_actions {
            return Err(err(Code::PolicyDenied, format!("The session reached its limit of {} actions.", p.max_actions)));
        }
        let (decision, reason) = p.evaluate(op, args)?;
        let id = new_id("act");
        let (state, error_code, final_reason) = match decision {
            Decision::Deny => ("denied", Some(Code::PolicyDenied.as_str()), reason.clone()),
            Decision::RequireApproval => ("awaiting_approval", None, reason.clone()),
            Decision::Allow => ("failed", Some(Code::EngineUnavailable.as_str()), format!("{reason} {NOT_RUN}")),
        };
        tx.execute(
            "INSERT INTO actions(id, session_id, app_id, idempotency_key, operation_id, args_hash, args, decision, state, reason, error_code, policy_revision, created_at, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?13)",
            params![id, session_id, app_id, key, op, args_hash, policy::canonical(args).to_string(), decision.as_str(), state, final_reason, error_code, rev, now],
        )?;
        tx.execute("UPDATE sessions SET action_count=action_count+1, updated_at=?1 WHERE id=?2", params![now, session_id])?;
        event(&tx, Some(session_id), Some(&id), "action.proposed", "requester", Some(rev), json!({"operation_id": op, "args_hash": args_hash}))?;
        event(&tx, Some(session_id), Some(&id), "action.decided", "engine", Some(rev), json!({"decision": decision.as_str(), "reason": reason}))?;
        match decision {
            Decision::RequireApproval => {
                let nonce = hex::encode(rand::random::<[u8; 16]>());
                tx.execute(
                    "INSERT INTO approvals(id, action_id, session_id, args_hash, policy_revision, nonce, expires_at, state) VALUES (?1,?2,?3,?4,?5,?6,?7,'pending')",
                    params![new_id("apr"), id, session_id, args_hash, rev, nonce, now + p.approval_minutes as i64 * 60_000],
                )?;
            }
            Decision::Allow => event(&tx, Some(&session_id.to_string()), Some(&id), "action.not_executed", "engine", Some(rev), json!({"reason": NOT_RUN}))?,
            Decision::Deny => {}
        }
        tx.commit()?;
        action_view(&c, app_id, &id)
    }

    pub fn actions_get(&self, app_id: &str, id: &str) -> Result<Value> {
        let c = self.conn();
        action_view(&c, app_id, id)
    }

    // ---------------------------------------------------------------- approvals

    pub fn approvals_list(&self) -> Result<Vec<Value>> {
        let c = self.conn();
        let mut s = c.prepare(
            "SELECT p.id, p.action_id, p.session_id, a.app_id, a.operation_id, a.args, p.policy_revision, p.expires_at, p.nonce
             FROM approvals p JOIN actions a ON a.id=p.action_id WHERE p.state='pending' AND p.expires_at > ?1 ORDER BY p.expires_at",
        )?;
        let rows = s.query_map([now_ms()], |r| {
            Ok(json!({"approval_id": r.get::<_, String>(0)?, "action_id": r.get::<_, String>(1)?, "session_id": r.get::<_, String>(2)?,
                "app_id": r.get::<_, String>(3)?, "operation_id": r.get::<_, String>(4)?,
                "arguments": serde_json::from_str::<Value>(&r.get::<_, String>(5)?).unwrap_or(Value::Null),
                "policy_revision": r.get::<_, i64>(6)?, "expires_at": r.get::<_, i64>(7)?, "nonce": r.get::<_, String>(8)?}))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Single-use decision bound to the exact action, arguments, policy revision and nonce.
    pub fn approvals_resolve(&self, approval_id: &str, nonce: &str, approve: bool) -> Result<Value> {
        let mut c = self.conn();
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(String, String, String, i64, String, i64, String)> = tx
            .query_row("SELECT action_id, session_id, args_hash, policy_revision, nonce, expires_at, state FROM approvals WHERE id=?1", [approval_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
            })
            .optional()?;
        let (action_id, session_id, args_hash, rev, real_nonce, expires, state) = row.ok_or_else(|| err(Code::NotFound, "No such approval"))?;
        if state != "pending" {
            return Err(err(Code::InvalidState, format!("This approval was already {state}.")));
        }
        if nonce != real_nonce {
            return Err(err(Code::StaleResource, "This approval doesn't match the current request. Review it again."));
        }
        let now = now_ms();
        let (a_hash, a_state, a_rev): (String, String, i64) = tx.query_row("SELECT args_hash, state, policy_revision FROM actions WHERE id=?1", [&action_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        let session_state: String = tx.query_row("SELECT state FROM sessions WHERE id=?1", [&session_id], |r| r.get(0))?;
        let stale = a_hash != args_hash || a_rev != rev || a_state != "awaiting_approval" || session_state != "running";
        if expires <= now || stale {
            let why = if expires <= now { "expired" } else { "stale" };
            tx.execute("UPDATE approvals SET state=?1, resolved_at=?2 WHERE id=?3", params![why, now, approval_id])?;
            tx.execute("UPDATE actions SET state='cancelled', updated_at=?1 WHERE id=?2 AND state='awaiting_approval'", params![now, action_id])?;
            event(&tx, Some(&session_id), Some(&action_id), &format!("approval.{why}"), "engine", Some(rev), json!({"approval_id": approval_id}))?;
            tx.commit()?;
            return Err(err(Code::StaleResource, format!("This approval is {why}; nothing was run.")));
        }
        let (ap_state, act_state, code, reason) = if approve {
            ("approved", "failed", Some(Code::EngineUnavailable.as_str()), format!("Approved by you. {NOT_RUN}"))
        } else {
            ("denied", "denied", Some(Code::PolicyDenied.as_str()), "You declined this action.".to_string())
        };
        tx.execute("UPDATE approvals SET state=?1, resolved_at=?2 WHERE id=?3", params![ap_state, now, approval_id])?;
        tx.execute("UPDATE actions SET state=?1, error_code=?2, reason=?3, updated_at=?4 WHERE id=?5", params![act_state, code, reason, now, action_id])?;
        event(&tx, Some(&session_id), Some(&action_id), &format!("approval.{ap_state}"), "user", Some(rev), json!({"approval_id": approval_id}))?;
        if approve {
            event(&tx, Some(&session_id), Some(&action_id), "action.not_executed", "engine", Some(rev), json!({"reason": NOT_RUN}))?;
        }
        tx.commit()?;
        Ok(json!({"approval_id": approval_id, "state": ap_state, "action_id": action_id}))
    }

    // ---------------------------------------------------------------- events

    pub fn events_read(&self, app_id: &str, session_id: &str, cursor: Option<&str>, limit: usize) -> Result<Value> {
        let c = self.conn();
        owned_session_state(&c, app_id, session_id)?;
        let after: i64 = match cursor {
            None | Some("") => 0,
            Some(s) => s.strip_prefix("c").and_then(|n| n.parse().ok()).ok_or_else(|| err(Code::InvalidArguments, "cursor isn't valid"))?,
        };
        let limit = limit.clamp(1, 200) as i64;
        let mut s = c.prepare("SELECT seq, event_id, action_id, ts, type, source, policy_revision, payload FROM events WHERE session_id=?1 AND seq > ?2 ORDER BY seq LIMIT ?3")?;
        let rows: Vec<(i64, Value)> = s
            .query_map(params![session_id, after, limit], |r| {
                let seq: i64 = r.get(0)?;
                Ok((seq, json!({"event_id": r.get::<_, String>(1)?, "engine_id": self.engine_id, "session_id": session_id, "action_id": r.get::<_, Option<String>>(2)?,
                    "sequence": seq, "ts": r.get::<_, i64>(3)?, "schema_version": 1, "type": r.get::<_, String>(4)?, "evidence_source": r.get::<_, String>(5)?,
                    "policy_revision": r.get::<_, Option<i64>>(6)?, "payload": serde_json::from_str::<Value>(&r.get::<_, String>(7)?).unwrap_or(Value::Null)})))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let next = rows.last().map(|(s, _)| *s).unwrap_or(after);
        Ok(json!({"events": rows.into_iter().map(|(_, v)| v).collect::<Vec<_>>(), "next_cursor": format!("c{next}"), "gap": false}))
    }
}

fn decode_key(k: &str) -> Result<VerifyingKey> {
    let bytes: [u8; 32] = b64().decode(k).ok().and_then(|b| b.try_into().ok()).ok_or_else(|| err(Code::InvalidArguments, "public_key must be a base64 Ed25519 key"))?;
    VerifyingKey::from_bytes(&bytes).map_err(|_| err(Code::InvalidArguments, "public_key isn't a valid Ed25519 key"))
}

fn current_policy(c: &Connection, profile: &str) -> Result<(i64, Policy)> {
    let row: Option<(i64, String)> = c.query_row("SELECT revision, body FROM policy_revisions WHERE profile=?1 ORDER BY revision DESC LIMIT 1", [profile], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
    let (rev, body) = row.ok_or_else(|| err(Code::NotFound, format!("No profile called \"{profile}\"")))?;
    let p = serde_json::from_str(&body).map_err(|e| err(Code::AuditUnavailable, format!("Stored policy can't be read: {e}")))?;
    Ok((rev, p))
}

fn policy_at(c: &Connection, profile: &str, rev: i64) -> Result<Policy> {
    let body: String = c.query_row("SELECT body FROM policy_revisions WHERE profile=?1 AND revision=?2", params![profile, rev], |r| r.get(0))?;
    serde_json::from_str(&body).map_err(|e| err(Code::AuditUnavailable, format!("Stored policy can't be read: {e}")))
}

/// Another app's session looks exactly like a missing one.
fn owned_session_state(c: &Connection, app_id: &str, id: &str) -> Result<String> {
    c.query_row("SELECT state FROM sessions WHERE id=?1 AND app_id=?2", params![id, app_id], |r| r.get(0)).optional()?.ok_or_else(|| err(Code::NotFound, "No such session"))
}

fn session_view(c: &Connection, app_id: &str, id: &str) -> Result<Value> {
    let v = c
        .query_row("SELECT id, profile, policy_revision, grant_id, state, created_at, started_at, action_count FROM sessions WHERE id=?1 AND app_id=?2", params![id, app_id], |r| {
            Ok(json!({"id": r.get::<_, String>(0)?, "profile": r.get::<_, String>(1)?, "policy_revision": r.get::<_, i64>(2)?, "resource_grant_id": r.get::<_, String>(3)?,
                "state": r.get::<_, String>(4)?, "created_at": r.get::<_, i64>(5)?, "started_at": r.get::<_, Option<i64>>(6)?, "action_count": r.get::<_, i64>(7)?,
                "coverage": {"filesystem": "not_enforced", "process": "not_enforced", "network": "not_enforced", "remote_tools": "not_enforced"},
                "execution": "unavailable"}))
        })
        .optional()?;
    v.ok_or_else(|| err(Code::NotFound, "No such session"))
}

fn action_view(c: &Connection, app_id: &str, id: &str) -> Result<Value> {
    let v = c
        .query_row(
            "SELECT a.id, a.session_id, a.operation_id, a.decision, a.state, a.reason, a.error_code, a.policy_revision, a.args_hash,
                    (SELECT id FROM approvals WHERE action_id=a.id) FROM actions a WHERE a.id=?1 AND a.app_id=?2",
            params![id, app_id],
            |r| {
                Ok(json!({"id": r.get::<_, String>(0)?, "session_id": r.get::<_, String>(1)?, "operation_id": r.get::<_, String>(2)?,
                    "decision": r.get::<_, String>(3)?, "state": r.get::<_, String>(4)?, "reason": r.get::<_, String>(5)?,
                    "error_code": r.get::<_, Option<String>>(6)?, "policy_revision": r.get::<_, i64>(7)?, "args_hash": r.get::<_, String>(8)?,
                    "approval_id": r.get::<_, Option<String>>(9)?, "executed": false, "evidence_source": "engine"}))
            },
        )
        .optional()?;
    v.ok_or_else(|| err(Code::NotFound, "No such action"))
}
