# AgentTracker

**Did your AI agent really do what it said it did?** AgentTracker records what local AI coding
agents (Claude Code, Cursor, …) report while they work, takes a fingerprint snapshot of your folders
before and after, and runs simple checks to tell you — in plain language — whether the job was
actually done.

> ⚠️ Read [KNOWN_LIMITATIONS.md](KNOWN_LIMITATIONS.md) before relying on a verdict.

## What it is
- A Windows desktop app (Tauri 2, Rust) with a tray icon and a friendly 7-screen UI:
  **Home, Connections, Create Task, Live Task, Results, Saved Checks, Settings**.
- A tiny hook program (`agenttracker-hook`) your agent calls on each event. It signs the event and
  hands it to the app over 127.0.0.1; if the app isn't running it saves a redacted copy to replay later.
- A verifier: "before" and "after" SHA-256 manifests + checks such as *Originals unchanged*,
  *Required outputs exist*, *Every input made it to the output*, *Folder looks right*,
  *Changes stayed in bounds*. Verdicts are **Passed**, **Failed** or **Unable to verify** — missing
  or unreadable evidence is never treated as a pass.
- Local-only: SQLite database in `%LOCALAPPDATA%\com.agenttracker.app`, secrets redacted before
  storage, events linked in a SHA-256 hash chain.

## What it is not
- **Not an enforcement tool** — it never blocks or undoes agent actions.
- **Not a backup** — snapshots are fingerprints, not copies; nothing can be restored from them.
- **Not tamper-proof** — the hash chain catches accidental edits, not a hostile program on your PC.
- **Not cross-platform** — Windows only (Linux/macOS unsupported).
- **Not a cloud service** — nothing leaves your computer.

## Repository layout
```
crates/tracker-common/      redaction, canonical JSON, HMAC/loopback security, machine id, license stub, hash chain
crates/agenttracker-core/   DB + migrations, event & task model, FS observer, verifiers, connectors, queue, collector, HTTP server
crates/agenttracker-hook/   the hook binary agents invoke
src-tauri/                  Tauri 2 shell: tray (Open / Pause Collection / Quit), window, settings
static/                     HTML/CSS/JS UI (no build step)
migrations/                 embedded SQL migrations v001..v003
tests/                      acceptance tests (fake-agent contract tests)
demo/                       harmless demo task — see demo/README.md
.github/workflows/build.yml Windows build + NSIS/MSI installers
```

## Building the Windows installer (GitHub Actions)
The workflow `.github/workflows/build.yml` runs on `windows-latest`, targets
`x86_64-pc-windows-msvc`, runs the tests, builds the release binaries, bundles the hook next to the
app, and produces `AgentTracker_<version>_x64-setup.exe` (NSIS) and
`AgentTracker_<version>_x64_en-US.msi`, uploaded as the **AgentTracker-windows-x64** artifact.

Configure these under **Settings → Secrets and variables → Actions** of the GitHub repo:

| Name | Kind | Required | Purpose |
|---|---|---|---|
| `AGENTTRACKER_API_KEY` | Secret | Yes (for release builds) | AgentTracker's own API key, baked in at build time. **Different from AI Tracker Lite's key.** If unset the build still works with a clearly-marked placeholder. |
| `AGENTTRACKER_LICENSE_URL` | Secret *or* Variable | Optional | License server URL baked into the build (default `https://license.invalid/agenttracker`; server wiring is a stub). |
| `TAURI_PRIVATE_KEY` | Secret | Optional | Tauri signing key (passed as both `TAURI_PRIVATE_KEY` and `TAURI_SIGNING_PRIVATE_KEY`). Only needed once the updater is enabled. |
| `TAURI_KEY_PASSWORD` | Secret | Optional | Password for `TAURI_PRIVATE_KEY`, if it has one. |

Trigger it by pushing to `main`, pushing a `v*` tag, or **Actions → Build Windows installer → Run workflow**.

## Running locally (development)
Requires Rust stable. On Windows nothing else is needed (WebView2 ships with Windows 10/11).
```bash
cargo test --workspace                      # unit + acceptance tests
cargo build -p agenttracker-hook            # build the hook (the app looks for it next to its exe)
cargo run -p agenttracker                   # desktop app: window + tray
cargo run -p agenttracker -- --headless     # backend only; prints a one-time UI link
```
Environment overrides: `AGENTTRACKER_HOME` (default `%USERPROFILE%\.agenttracker`, holds the hook
discovery file + connector state) and `AGENTTRACKER_DATA_DIR` (database and demo files).

Security model (same pattern as AI Tracker Lite): the server binds a random port on 127.0.0.1 only,
checks the `Host` header, the window opens a **one-time launch token** that becomes an HttpOnly
`SameSite=Strict` session cookie, state-changing UI calls need the `x-requested-with: agenttracker`
header, pages get a strict CSP, and the hook endpoint requires an HMAC-SHA256 signature with a
per-run secret stored in `~/.agenttracker/collector.json`.

## Connecting agents
Use **Connections** in the app — it does the steps below for you, keeps a one-time backup, is safe
to run repeatedly, and **Disconnect** restores your file (or removes only AgentTracker's entries
if you've edited it since). Restart the agent afterwards.

### Claude Code (implemented — needs live validation)
Edits `%USERPROFILE%\.claude\settings.json`, adding to `hooks` for `SessionStart`,
`UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Notification`, `Stop` and `SessionEnd` an entry
(tool events get `"matcher": "*"`) such as:
```json
{ "matcher": "*", "hooks": [ { "type": "command", "timeout": 5,
  "command": "\"C:/Program Files/AgentTracker/agenttracker-hook.exe\" --connector claude_code --event PreToolUse --at-owner agenttracker" } ] }
```
The `--at-owner agenttracker` marker is how AgentTracker recognises (and only ever removes) its own entries.

### Cursor (experimental)
Edits `%USERPROFILE%\.cursor\hooks.json` (`{"version": 1, "hooks": {...}}`) with the same kind of
command for `sessionStart`, `postToolUse`, `afterShellExecution`, `afterMCPExecution`,
`afterFileEdit`, `stop` and `sessionEnd`. Coverage is partial; see KNOWN_LIMITATIONS.md.

### Manual / any other agent
No hooks. Create a task, take the "before" snapshot, let any tool (or yourself) do the work, then
click *check it*. File checks work the same.

## Try it
See [demo/README.md](demo/README.md) — **Home → Try the demo** creates everything for you.

## Known limitations
See [KNOWN_LIMITATIONS.md](KNOWN_LIMITATIONS.md): no live connector testing in the dev environment,
Cursor is experimental, hash chain ≠ protection from a hostile local agent, observational only,
no restore, Windows only, license server is a stub.
