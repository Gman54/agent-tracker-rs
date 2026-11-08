# Cursor connector

**Status:** **Experimental.** **Not live-tested.** It's been tested only with fake hook payloads in the documented format.

| | |
|---|---|
| Docs checked | <https://cursor.com/docs/agent/hooks> (2026-11-03; the page carries no version number) |
| Version | Not published. TrailFox shows `cursor --version` if it answers, and payloads include `cursor_version` |
| Config file | `%USERPROFILE%\.cursor\hooks.json` (`{"version": 1, "hooks": {...}}`) |
| Events registered | `sessionStart`, `postToolUse`, `postToolUseFailure`, `afterShellExecution`, `afterMCPExecution`, `afterFileEdit`, `stop`, `sessionEnd` |
| Hook command | `"<path>\agenttracker-hook.exe" --connector cursor --event <event> --at-owner agenttracker` |

## How install works
It works the same way as Claude Code: preview the diff, then connect (with a one-time backup, and safe to repeat). Disconnect
restores your file or removes only TrailFox's marked entries.

## What is captured
- Session start/end, finished tool calls, shell commands, MCP calls, file edits, and the agent
  stopping (its *claim* to be done).
- Payload fields used: `conversation_id`, `generation_id`, `hook_event_name`, `cursor_version`,
  `workspace_roots`, plus event-specific fields.

## What isn't captured
- **Your prompt** (`beforeSubmitPrompt`) and **tool start** (`preToolUse`). Those hooks can block
  Cursor, and TrailFox never registers blocking hooks.
- The transcript and model reasoning.

## Limitations
- Cursor's hook API is new and may change between releases. Treat agent activity as best effort.
  File checks are unaffected.
- The hook always exits 0 and prints nothing.
