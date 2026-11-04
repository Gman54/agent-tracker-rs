# AgentTracker: status

Last updated: 2026-11-04 (Phase 3). Every line below says exactly how far that feature has been checked.
"Fake-agent tested" means an automated test sends hand-built hook payloads in the documented
format and does real file work. **No real Claude Code, Cursor or Copilot process was involved.**

**Automated tests:** `cargo test --workspace` → **39 passed, 0 failed** (29 acceptance tests,
3 core unit tests and 7 tracker-common unit tests). `cargo check --workspace` is clean. Both were
run on Linux in the dev sandbox; the Windows CI run happens in GitHub Actions.

| Feature | Status | How it was checked |
|---|---|---|
| Hash chain (SHA-256, tamper-evident) | Implemented | Unit + acceptance tested (`test_hash_chain_append_and_verify`); UI "Check history" button returns `{ok, checked, first_bad_seq}` |
| HTTP collector, loopback auth (Host check, one-time launch token → cookie, CSRF header, HMAC hook signature) | Implemented | Unit tested + `test_unauthorized_requests_rejected` (in-process HTTP) |
| Durable queue + hook fallback file | Implemented | `test_queue_enqueue_drain_deliver`, `test_collector_outage_recovery`, `test_hook_exits_zero_when_collector_down` (runs the real hook binary) |
| Verifier: originals_unchanged | Implemented | Fake-agent tested |
| Verifier: required_outputs_exist | Implemented | Fake-agent tested |
| Verifier: inputs_preserved_in_output | Implemented | Fake-agent tested |
| Verifier: folder_structure_matches | Implemented | Shown passing in the browser demo task. **No dedicated automated test yet** |
| Verifier: changes_within_scope | Implemented | Fake-agent tested |
| Verify-now dedupe (30 s idempotency key) | Implemented | `test_verify_now_dedupe` |
| Frozen task definition per verification | Implemented | `test_frozen_definition_used_during_verify` |
| Restart gap recording / resume watching | Implemented | `test_watcher_interruption_gap_recorded` |
| Claude Code connector | Implemented | Install/uninstall/normalize fake-agent tested. **Not live-tested** |
| Cursor connector | Implemented, **Experimental** | Fake-agent tested. **Not live-tested** |
| GitHub Copilot (VS Code agent) connector | Implemented, **Experimental** | 4 fake-agent tests (install idempotent, uninstall preserves unrelated, payload normalization, unsupported version). **Not live-tested** |
| Manual mode (just a folder) | Implemented | Fake-agent tested; browser demo run end to end |
| Codex connector | Not started | Planned post-release |
| UI: 7 screens (Home, Connections, New Task, Live Task, Results, Saved Checks, Settings) | Implemented | Browser-verified on loopback (headless app, Chrome). No console errors. Screenshots in `docs/screenshots/` |
| Welcome wizard (3 steps) | Implemented | Browser-verified |
| Report export (HTML / JSON) | Implemented | API checked on loopback (200, attachment download). Not opened in a desktop download flow |
| Create check from incident | Implemented | `/api/ui/incident/propose` maps a failed check to a saved check. API checked; returns a plain-English "nothing to turn into a rule" message for passed checks |
| Pause / resume collection | Implemented | Browser-verified (sidebar pill + banner) |
| Data retention / clean-up | Implemented | API wired; not covered by an automated test |
| Custom redaction phrases | Implemented | Wired in collector; not covered by an automated test |
| System tray (Open / Pause / Quit), close-to-tray setting | Implemented | **Not tested (no display in the sandbox)** |
| Start with Windows | Setting stored + registry code | **Not tested (Linux sandbox)** |
| Auto-verify when the agent says it's done | Setting stored only | **Not wired yet** |
| Native folder picker | Desktop shell hook only | Headless/browser falls back to typing a path |
| Default export folder | Setting stored only | Downloads go to the browser/WebView download location |
| Windows installer (NSIS / MSI) | Not built here | Delivered via GitHub Actions (`.github/workflows/build.yml`) |
| Live Claude Code test | Not done | Needs a live agent |
| Live Cursor test | Not done | Needs a live agent |
| Live Copilot test | Not done | Needs a live agent |
| License server | Stub | Format check only (see KNOWN_LIMITATIONS.md) |
| Decision runtime (`agenttracker-runtime`) | Implemented | **23 simulated tests pass** (`cargo test -p agenttracker-runtime`, scripted provider). See docs/runtime/CAPABILITY_REPORT.md |
| Runtime live model test | Partial | Ollama `qwen2.5:1.5b` (CPU): 3 runs, **no valid tool action produced**. Not supported. Other models untested |
| Runtime with LM Studio | Not tested | No LM Studio server available in the sandbox |
| Runtime replay key DPAPI wrapping | Written | `cfg(windows)`, **not compiled in the sandbox** |
| Agent Runtime screen + "Why this action?" panel | Implemented | Browser-verified on the headless preview server; screenshots in docs/runtime/screenshots/ |
