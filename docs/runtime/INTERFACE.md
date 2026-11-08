# Decision runtime: interface

Crate: `crates/agenttracker-runtime` (separate from `agenttracker-core`). Schema: `migrations/v004_runtime.sql`.
Runtime version string recorded on every model request: `agenttracker-runtime/0.1.0+schema.v004`.

## What the records are, and are not

The runtime records:

- the application-level request it sent to the model, including the context items it listed;
- the model's structured decision proposal, including its **stated rationale**;
- validation attempts, the accepted decision, and the approval if one was needed;
- the action, the tool-reported result, and file changes that TrailFox observed independently.

It does **not** record internal model computation, provider-internal chat templates or hidden
prompts, or tokenisation. Nothing in these records proves which input caused an action. A citation
shows what the model referred to, not what caused its output.

## Flow (one step)

```
context snapshot ─► model request ─► proposal attempt(s) ─► validation ─► accepted decision (persisted)
                                         │ invalid → bounded repair (max_validation_retries)
                                         ▼
                         approval (if policy requires) ─► action: prepared ─► started ─► succeeded | failed
                                                                                 └─(crash)─► unknown (never auto-repeated)
                                                         ─► result (tool-reported) + changes (observed by hashing)
                                                         ─► deterministic checks ─► findings
```

A tool runs only after its decision row has been committed. The dispatcher re-checks path
confinement and protected paths itself; it does not trust the validator.

## Rust API

| Item | Purpose |
|---|---|
| `runtime::Runtime::new(db, provider, cfg, vault)` | Build a runtime over the shared TrailFox `Db` |
| `Runtime::start_task(&TaskSpec) -> StartOutcome` | Creates task, session and context items. `idempotency_key` returns the existing session instead of starting a duplicate |
| `Runtime::run(session_id) -> SessionStatus` | Runs steps until done, paused (needs approval/clarification, limits, invalid proposals) or failed |
| `Runtime::approve(decision_id, who)` | Records an approval for the exact `args_sha256`. Different arguments need a new decision |
| `Runtime::recover_unknown(&Db)` | On startup: `started` without a result becomes `unknown` (with a reconciliation verification); `prepared` becomes `failed` (it never ran) |
| `provider::Provider` | Trait: `chat`, `info`, `settings` |
| `provider::OpenAiCompatProvider` | Local OpenAI-compatible endpoint (Ollama, LM Studio, others) |
| `provider::ScriptedProvider` | Recorded fixtures and simulated tests. Always reported as a fixture |
| `tools::dispatch(tool, args, root, protected)` | Confined tool execution |
| `why::bundle_for_action / bundle_for_decision` | Data for the "Why this action?" panel |
| `checks::*` | Deterministic contradiction checks, plus optional `analyze_with_model` (labelled inferred) |
| `investigate::run` | "Test this influence" replays (decision stage only, no tools) |
| `replay_vault::ReplayVault` | Optional encrypted store for exact payloads |
| `http::routes()` | Axum routes mounted by the desktop shell via `server::serve_with` |

## Tools (the only actions a runtime model can take)

| Tool | Arguments | Writes | Idempotent | Approval by default |
|---|---|---|---|---|
| `list_directory` | `path` | no | yes | no |
| `read_text_file` | `path` (first 32 KB) | no | yes | no |
| `copy_file` | `source`, `destination` (fails if the destination exists) | `destination` | no | no |
| `write_text_file` | `path`, `content` | `path` | no | **yes** |

Every path is relative to the session's working folder. The dispatcher rejects absolute paths, `..`,
symlink escapes and writes under protected paths.

## Identifiers

The runtime generates every ID. Format: `<prefix>_<UUIDv7 as 32 hex chars>`. UUIDv7 is time-ordered
with 74 random bits, so IDs are collision-resistant; their time order is not a causal claim. Any ID the model supplies (for example a
`decision_id` field in its JSON) is ignored and noted on the decision.

| Prefix | Entity | Prefix | Entity |
|---|---|---|---|
| `task` | task_id | `act` | action_id |
| `ses` | session_id (`parent_session_id` for subagents) | `res` | result_id |
| `snap` | context_snapshot_id | `chg` | change_id |
| `req` | model_request_id | `ver` | verification_id |
| `ins` | instruction_id | `inv` | investigation_id |
| `evd` | evidence_id | `att` | proposal attempt |
| `dec` | decision_id | `apr`, `fnd`, `trl` | approval, finding, replay trial |

The model sees stable **reference labels** (`instruction_1`, `evidence_2`, …) scoped to the session.
The label-to-ID mapping is stored in `rt_snapshot_items`. Each snapshot records which items were
`included` and which were omitted (with a reason), so evidence dropped from a later context is
never shown as available.

## Proposal schema (what the model must return)

```json
{
  "instruction_refs": ["instruction_2"],
  "evidence_refs": ["evidence_1"],
  "stated_rationale": "…",
  "assumptions": ["…"],
  "proposed_action": {"tool": "copy_file", "arguments": {"source": "…", "destination": "…"}},
  "expected_effect": "…",
  "uncertainty": ["…"],
  "alternatives_considered": ["…"],
  "needs_clarification": false,
  "response_text": "required when proposed_action is null"
}
```

Validation rejects malformed JSON, unknown tools, missing or extra arguments, refs that do not
exist, refs that were not included in this request, paths outside the folder or under a protected
path, and a null action that has neither `response_text` nor `needs_clarification`. Rejected
attempts are kept in `rt_proposal_attempts`.

Provider-exposed reasoning (`reasoning_content` / `reasoning` fields, when a server sends them) is
stored separately as optional "provider-exposed reasoning". It never replaces the stated rationale.

## Tables (v004)

`rt_tasks`, `rt_sessions`, `rt_context_items`, `rt_context_snapshots`, `rt_snapshot_items`,
`rt_model_requests`, `rt_proposal_attempts`, `rt_decisions`, `rt_approvals`, `rt_actions`,
`rt_action_states`, `rt_results`, `rt_verifications`, `rt_changes`, `rt_findings`,
`rt_investigations`, `rt_replay_trials`, `rt_replay_store`.

Append-only triggers cover context items, attempts, decisions, approvals, actions, action states,
results, changes, verifications, findings and trials. Corrections are new records: a retrospective
explanation is a new `kind='retrospective'` decision with `revises_decision_id`, and the original
stated rationale stays unchanged. Every inserted record is also appended to the existing
`chain_entries` hash chain in the same transaction.

## HTTP routes (desktop UI only)

All routes sit behind the existing core guards: Host check, launch-token cookie and the
`x-requested-with: agenttracker` CSRF header.

| Method | Route | |
|---|---|---|
| GET/POST | `/api/ui/runtime/config` | Provider and limits. The API key is stored separately and never returned |
| POST | `/api/ui/runtime/provider/test` | Lists models. Reachable does not mean supported |
| GET | `/api/ui/runtime/sessions`, `/sessions/:id` | Runtime sessions only (origin `agenttracker_runtime`) |
| POST | `/api/ui/runtime/tasks`, `/sessions/:id/resume` | Start or resume |
| POST | `/api/ui/runtime/decisions/:id/approve` | Approve the exact arguments |
| GET | `/api/ui/runtime/actions/:id/why`, `/decisions/:id/why` | "Why this action?" bundle |
| POST | `/api/ui/runtime/decisions/:id/analyze` | Optional inferred analysis (appends a finding only) |
| POST/GET | `/api/ui/runtime/investigations`, `/investigations/:id` | Influence test |
| POST | `/api/ui/runtime/demo/a`, `/demo/b` | `{"mode":"fixture"}` (default) or `{"mode":"live"}` |
| POST | `/api/ui/runtime/replay/delete`, `/recover` | Delete replay captures; reconcile interrupted actions |

## Relationship to existing TrailFox features

- **Reused:** the `Db` and migration runner, redaction (`tracker_common::redaction`), the hash chain,
  the folder `Snapshotter` (before/after hashing), the loopback server guards and the UI shell.
- **Separate:** runtime sessions live in `rt_*` tables and the "Agent Runtime" screen. Externally
  connected agents (Claude Code, Cursor, Copilot hooks) keep their own tables, connectors and
  screens, which are unchanged.

## Enforcement boundary and threat model

- Model output, tool output and document content are untrusted data. Document text is shown to the
  model inside labelled context items. Nothing in it can change limits, approvals, protected paths
  or records (see the `prompt_injection_in_evidence_cannot_alter_permissions_or_audit_records` test).
- Confinement is enforced by the dispatcher, not by the model or the validator alone.
- The hash chain and append-only triggers give tamper **evidence** against accidental or casual
  edits. Any process running as the same user can rewrite the SQLite file and rebuild the chain.
  This is not a security boundary.
- Replay captures (exact, unredacted payloads) are off by default. When enabled they are encrypted
  with ChaCha20-Poly1305. On Windows the key is wrapped with DPAPI (current user). That code is
  `cfg(windows)` and **was not compiled in this development environment**. On other platforms the
  key is a 0600 file. Normal record views never read the vault.
