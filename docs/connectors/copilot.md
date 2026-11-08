# GitHub Copilot (VS Code agent) connector

**Status:** **Experimental.** **Not live-tested.** It's been tested only with fake payloads and temp config files.
VS Code agent hooks are themselves a *Preview* feature.

| | |
|---|---|
| Docs checked | <https://code.visualstudio.com/docs/copilot/customization/hooks> and <https://docs.github.com/en/copilot/reference/hooks-reference> (2026-11-03) |
| Version | Neither page publishes a minimum version. TrailFox shows `code --version` if VS Code is on PATH |
| Config file | `%USERPROFILE%\.copilot\hooks\agenttracker.json` (`%COPILOT_HOME%\hooks\` if `COPILOT_HOME` is set). TrailFox uses **its own file** and never edits your other hook files |
| Format | `{"version": 1, "hooks": {"<event>": [{"type": "command", "bash": "...", "powershell": "& ...", "timeoutSec": 5}]}}` |
| Events registered | `sessionStart`, `userPromptSubmitted`, `postToolUse`, `agentStop`, `sessionEnd` |

## How install works
It works the same way as the other connectors: preview, connect (safe to repeat), disconnect. Because TrailFox
writes a separate `agenttracker.json`, disconnecting only touches that file. If you added your
own entries to it, they're kept.

## What is captured
- Session start/end, the prompt (redacted), finished tool calls, and the agent stopping (its *claim*).
- Both payload shapes are understood: camelCase (`sessionId`, `timestamp`, `cwd`, `toolName`,
  `toolArgs`, `toolResult`) and the PascalCase-alias snake_case shape (`session_id`,
  `hook_event_name`, `tool_name`, ...). `transcriptPath` is dropped.

## What isn't captured
- **Tool start** (`preToolUse`) and `permissionRequest`. They can deny tool calls and are
  fail-closed if a hook crashes, so TrailFox never registers them.
- The transcript and model reasoning.

## Unsupported versions
A payload without `sessionId` or `timestamp` is stored as a capture error flagged
`unsupported_payload`. Connections then shows **"Unsupported version"** in plain words. File checks
still work.

## Limitations
- Workspace trust and organization policy can turn hooks off, and TrailFox can't see that setting.
- The hook always exits 0 and prints nothing.
