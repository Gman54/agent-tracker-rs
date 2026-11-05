# AgentTracker: status

Last updated: 2026-11-04 (Code changes / Git tracking). Every line below says exactly how far that feature has been checked.
"Fake-agent tested" means an automated test sends hand-built hook payloads in the documented
format and does real file work. **No real Claude Code, Cursor or Copilot process was involved.**

**Automated tests:** `cargo test --workspace --exclude agenttracker` → **94 passed, 0 failed**
(including 18 Git tracking acceptance tests, 14 core unit tests, 23 runtime tests and 7
tracker-common unit tests). `cargo check -p agenttracker` and
`cargo check -p agenttracker-core --target x86_64-pc-windows-gnu` are clean. All of this was run on
Linux in the dev sandbox; the Windows CI run happens in GitHub Actions.

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
| UI: 8 screens (Home, Connections, New Task, Live Task, Results, Saved Checks, Code changes, Settings) | Implemented | Browser-verified on loopback (headless app, Chrome). No console errors. Screenshots in `docs/screenshots/` (Code changes was checked separately on the headless preview; no screenshot saved) |
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
| Git tracking (Code changes screen): agent edits matched to commits | **Supported** (fake-agent) | The three required cases pass with real git repos and a local bare remote: agent edits and pushes, agent edits and a human pushes, a human revises the agent's edit before committing (`tests/tests/git_tracking.rs`; the original 12 passed 3 runs in a row, and all 18 including the fetch tests pass now). Partial staging, mixed commits, missing evidence, failed pushes and `echo git push` are also tested. Commit screen with "What the agent proposed" vs "What actually shipped" browser-verified on the headless preview. **Not live-tested with a real agent or on Windows** |
| Four separate facts (edit attribution, commit authorship, agent ran git push, remote branch updated) | Implemented | Asserted per case in the acceptance tests; unknown stays "Unknown" / "Not seen" |
| "Check the remote now" (git fetch from the Code changes screen) | Implemented | `RepoStore::fetch` runs `git fetch --all --prune` with a 2-minute limit and plain-English errors (sign-in, unreachable, project not found, no remote, folder missing). Three acceptance tests use a second clone and a local bare remote: a push made elsewhere turns the remote fact from "Not seen" to "Confirmed", a second check reports "Already up to date", local files and branches are unchanged. Buttons on the commit list and the commit page browser-checked on the headless preview. **Not tested against github.com or on Windows** |
| "Check all projects" (git fetch for every project) | Implemented | `RepoStore::fetch_all` checks every project, up to 4 at a time, skipping projects with no remote and continuing past failures; `POST /api/ui/git/fetch-all`. Three acceptance tests: a mix of a project with a push from elsewhere, an up-to-date project, a no-remote project and a broken remote (each gets the right result, "Last checked" saved only for the successful ones, local files unchanged), a deleted project folder, no projects, and more projects than workers. Button, "Checking 3 projects..." state, summary and per-project lines browser-checked on the headless preview. **Not tested against github.com or on Windows** |
| GitHub commit and pull request links (optional, read-only token) | Implemented | Tested against a **local mock server only** (checks the token is sent, no prompt text is sent, token not in the report). **Not tested against the real GitHub API** |
| Open GitHub link in your browser | Implemented | **Not tested** (button not clicked in the preview; Windows `rundll32` path not run) |
| Agent Runtime screen + "Why this action?" panel | Implemented | Browser-verified on the headless preview server; screenshots in docs/runtime/screenshots/ |
