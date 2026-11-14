# Known limitations

Houndbit is honest about what it can and can't tell you. Please read this before relying on a verdict.

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

VS Code agent hooks (used by the Copilot connector) are a **Preview** feature. Houndbit never
registers `preToolUse` / `permissionRequest` (they can deny tool calls), so "tool started" isn't
captured. Workspace trust or organization policy can switch hooks off without Houndbit knowing.
Payloads missing `sessionId`/`timestamp` are shown as **Unsupported version**.

Codex is **not supported yet** (planned post-release); use manual mode.

## 3. The hash chain does not stop a hostile local program
Each stored event is linked into a SHA-256 hash chain, which detects accidental edits or partial
corruption of the event history. Anything running as your user (including an AI agent with shell
access) can rewrite the entire database **and** recompute the chain. The chain is tamper-*evident*
against casual edits only; it is not tamper-*proof*.

## 4. Observational only — no enforcement
Houndbit never blocks, pauses or undoes an agent's actions. Hooks always exit successfully so
they can't break your agent. It records and verifies after the fact.

## 5. No restore — a manifest is not a backup
Snapshots store file **fingerprints** (SHA-256, size, timestamps), not file contents. If an agent
deletes or damages a file, Houndbit can tell you *that* it happened, but can't bring it back.
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
- Changes made while Houndbit was closed aren't seen live; the Live Task screen lists this as
  a gap, and the final snapshot comparison still catches them.
- The UI is English only.

## 11. Decision runtime
- Records show the application-level request, the model's stated rationale and the observed
  execution. They do not show internal model computation, provider-side templates or hidden
  prompts, and they do not prove which input caused an action.
- No model is marked supported. `qwen2.5:1.5b` was tried live and did not produce a valid tool action.
- LM Studio is untested. Windows DPAPI key wrapping for replay captures has not been compiled.
- The "completion claim" check is a keyword heuristic and will miss rephrased claims.
- Influence tests replay only the decision stage, and seeds do not guarantee reproducibility.
- The runtime enforces protected paths for its own tools only. Externally connected agents are
  still observed, not controlled.

## 12. Code changes (Git tracking)
- Only edits made with the agent's file-edit tools are attributed (Claude Code Edit/Write-style
  tools, Cursor `afterFileEdit`, Copilot edit tools). Changes an agent makes through shell commands
  (`sed`, `echo > file`, scripts) are not attributed to it and show up as "work from someone else".
- A clean "before" snapshot is only taken for Claude Code (at `PreToolUse`). For other agents the
  before text comes from what the agent reported, or is missing, and the match basis is weaker.
- Reworded lines are matched by text similarity (60% or more). That is a judgement, not proof, and
  the screen says so.
- Rebase, squash, amend and cherry-pick give commits new ids. Matching still works line by line,
  but with lower confidence.
- "Remote branch updated" is checked against this computer's remote-tracking branches, which only
  move when you push or fetch here, or against GitHub if you turned that on for the project.
  **Check the remote now** runs `git fetch --all --prune` to refresh them. It relies on Git's own
  saved sign-in: it never asks for a password in a terminal, but on Windows Git Credential Manager
  may open its own sign-in window for an HTTPS remote. It stops after 2 minutes. It was tested only
  with local bare remotes, not against github.com or on Windows. "Last checked" only counts checks
  made from Houndbit, not fetches you run yourself.
  **Check all projects** runs the same check for every project, up to 4 at a time, each with its
  own 2-minute limit, so with many slow remotes it can take several minutes; the screen waits
  for all of them before showing the summary. If several projects use HTTPS remotes that need
  signing in, Windows may show more than one Git sign-in window. The result list is kept only
  until you leave the Code changes screen.
- "Agent ran git push" is based on the agent's own tool report. Whether it succeeded comes from the
  exit status the agent reported.
- Binary files and files over 512 KB are compared by Git blob id only, not line by line.
- The GitHub token is stored in Houndbit's local settings database without extra encryption.
  Use a fine-grained, read-only token limited to the projects you pick.
- GitHub links were tested against a local mock server only, not the real GitHub API. Nothing in
  this feature has been tested with a live agent or on Windows yet.

## 13. Check wording, written explanations and reasoning

- A check with nothing to look at shows **Not applicable** (for example, the agent reported no file edits). It never says "All 0 ... passed" and it never counts towards a pass.
- "The agent only changed files in the project folder" is based on the file edits the agent reports. When the agent ran shell commands (Bash, PowerShell and so on), it shows **Insufficient coverage**, because files a shell command creates or changes are invisible to it.
- When only checks that protect the originals passed, the result reads "Original files kept — the result itself wasn't checked" and is grey, not green. No built-in check judges whether an output (a PDF, a report) is complete or numerically correct. "Required files exist" only confirms that the file is there and not empty.
- Results saved by older versions are re-read with these rules when you open them. Only the wording and status change; files aren't checked again.
- The Conversation section shows Claude's **written explanations**: the text it wrote in the conversation. That is its stated justification, not verified reasoning, and it isn't hidden thinking. Thinking entries appear only when Claude Code saved them.
- "Claude's written note before this action" is linked by order: it's the text Claude wrote since your last message or the last tool result. Actions Claude starts together share the same note. Each action is linked to its tool-call id, which matches the start/finish records the hooks sent.
- A structured rationale captured *before* each action (linked to the instruction and the tool-call id) is only enforced in the Agent Runtime. For Claude Code, Houndbit can only read what Claude chose to write.
