# Known limitations

AgentTracker is honest about what it can and can't tell you. Please read this before relying on a verdict.

## 1. Connectors were not tested against live agents in this environment
The Claude Code, Cursor and GitHub Copilot (VS Code) connectors were written against their **published hook documentation**
(Claude Code: <https://docs.anthropic.com/en/docs/claude-code/hooks>; Cursor:
<https://cursor.com/docs/agent/hooks>; Copilot: <https://code.visualstudio.com/docs/copilot/customization/hooks>
and <https://docs.github.com/en/copilot/reference/hooks-reference>, all checked 2026-11-03). The automated tests use
**fake-agent contract tests**: hand-built hook payloads in the documented format. They prove the
collector and verifiers behave correctly for those payloads — they do **not** prove a real agent
sends exactly those payloads. Run *Connections → Test* and a demo task with your real agent before
trusting the integration.

## 2. Cursor and Copilot connectors are experimental
Cursor's hook API is newer and less stable. Event coverage is partial (see the connector's
"What I can see from this agent" list) and the format may change between Cursor releases. Treat Cursor results
as best-effort; file checks still work regardless.

VS Code agent hooks (used by the Copilot connector) are a **Preview** feature. AgentTracker never
registers `preToolUse` / `permissionRequest` (they can deny tool calls), so "tool started" isn't
captured. Workspace trust or organization policy can switch hooks off without AgentTracker knowing.
Payloads missing `sessionId`/`timestamp` are shown as **Unsupported version**.

Codex is **not supported yet** (planned post-release); use manual mode.

## 3. The hash chain does not stop a hostile local program
Each stored event is linked into a SHA-256 hash chain, which detects accidental edits or partial
corruption of the event history. Anything running as your user (including an AI agent with shell
access) can rewrite the entire database **and** recompute the chain. The chain is tamper-*evident*
against casual edits only; it is not tamper-*proof*.

## 4. Observational only — no enforcement
AgentTracker never blocks, pauses or undoes an agent's actions. Hooks always exit successfully so
they can't break your agent. It records and verifies after the fact.

## 5. No restore — a manifest is not a backup
Snapshots store file **fingerprints** (SHA-256, size, timestamps), not file contents. If an agent
deletes or damages a file, AgentTracker can tell you *that* it happened, but can't bring it back.
Keep real backups / version control.

## 6. Snapshot limits
- Content is compared at two moments (before / after). A change that was made and then reverted in
  between is invisible to file checks.
- Files that keep changing during capture are marked *unstable* and anything depending on them is
  reported as *Unable to verify*, never as passed.
- Locked or unreadable files are reported as *Unable to verify*.
- Symbolic links / junctions are recorded but never followed.
- Only folders added to the task are snapshotted; changes elsewhere on the PC aren't seen.
- Live file-change notifications are hints only; verdicts come from the snapshots.

## 7. Windows only
The product targets Windows 10/11 x64 and is built by GitHub Actions on Windows. Linux and macOS
are **unsupported**. (The code compiles and the test-suite runs on Linux for development, but the
desktop app, installer and connectors are not supported there.)

## 8. License server wiring is a stub
The license key is only checked for its **format** (`AT-XXXXX-XXXXX-XXXXX-XXXXX`). No server
activation, revocation or machine binding happens yet. `AGENTTRACKER_API_KEY` and
`AGENTTRACKER_LICENSE_URL` are baked into the build but not yet used for network calls.

## 9. Settings that are stored but not fully wired yet
- **Auto-verify when the agent says it's done** is saved but doesn't trigger a check yet; press
  *Verify now*.
- **Default export folder** is saved, but reports are downloaded to the normal download location.
- **Browse…** for folders opens a native picker only in the desktop app; in headless/browser mode
  you type the path.
- **Start with Windows** and **keep running in the tray** were written for Windows but not tested
  on a real Windows desktop.

## 10. Other
- The desktop shell (tray, window) and the NSIS/MSI installers are only produced in GitHub
  Actions; they were not built or run in the development sandbox.
- Event attribution to a task uses the agent's working directory; two tasks running in *different*
  folders are told apart (tested), but two tasks in the same folder at once can't be.
- Changes made while AgentTracker was closed aren't seen live; the Live Task screen lists this as
  a gap, and the final snapshot comparison still catches them.
- The UI is English only.
