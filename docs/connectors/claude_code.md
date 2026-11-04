# Claude Code connector

**Status:** Implemented. **Not live-tested.** It's been tested only with fake hook payloads in the documented format.

| | |
|---|---|
| Docs checked | <https://docs.anthropic.com/en/docs/claude-code/hooks> (2026-11-03) |
| Version | The docs don't publish a minimum version. AgentTracker shows whatever `claude --version` reports |
| Config file | `%USERPROFILE%\.claude\settings.json` (user-level settings) |
| Events registered | `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `Notification`, `Stop`, `SessionEnd` (tool events use `"matcher": "*"`) |
| Hook command | `"<path>\agenttracker-hook.exe" --connector claude_code --event <Event> --at-owner agenttracker` (timeout 5 s) |

## How install works
1. **Connections → Show what will change** shows the exact line diff before anything is written.
2. **Connect** adds only AgentTracker's entries and keeps a one-time backup of your original file.
   Running it again changes nothing.
3. **Disconnect** restores the backup if you haven't touched the file since. Otherwise it removes
   only entries carrying the `--at-owner agenttracker` marker, so your own hooks and settings stay.

## What is captured
- Session start/end, the prompt you gave (redacted), each tool call start and finish, and the
  agent saying it's done (`Stop`). That last one is the agent's *claim*; the files are checked separately.
- Payload fields used: `session_id`, `cwd`, `hook_event_name`, `tool_name`, `tool_input`,
  `tool_response`, `prompt`. `transcript_path` is dropped.

## What isn't captured
- The transcript and model reasoning.
- Anything Claude Code does without a hook (for example, edits made while hooks are disabled by policy).
- Changes by other programs. Those show up only in the file snapshot comparison and are never
  attributed to the agent.

## Limitations
- Hooks are observational. The hook always exits 0 and prints nothing, so it can't block Claude Code.
- If AgentTracker isn't running, events go to `~/.agenttracker/fallback_queue.jsonl` (redacted)
  and are replayed on next start.
- Event-to-task matching uses the agent's working folder.

## How Claude Code is detected

1. `claude --version` on the PATH the AgentTracker window sees.
2. If that fails: the usual install folders — `%USERPROFILE%\.local\bin\claude.exe` (native installer),
   `%USERPROFILE%\.claude\local\`, `%APPDATA%\npm\claude.cmd`, `%LOCALAPPDATA%\Microsoft\WinGet\Links\claude.exe`.
3. If no stand-alone `claude` is found: the Claude desktop app (`%LOCALAPPDATA%\AnthropicClaude`) or the
   Claude Code VS Code/Cursor extension counts as "found (built-in Claude Code)".

Hooks in `~/.claude/settings.json` are documented to apply to the desktop app too, but users have reported them
not firing there (anthropics/claude-code#30953). Not verified by us either way. The `claude` command in a terminal is
the most reliable way to get events.
