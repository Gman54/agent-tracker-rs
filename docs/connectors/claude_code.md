# Claude Code connector

**Status:** Implemented. **Not live-tested.** It's been tested only with fake hook payloads in the documented format.

| | |
|---|---|
| Docs checked | <https://docs.anthropic.com/en/docs/claude-code/hooks> (2026-11-03) |
| Version | The docs don't publish a minimum version. TrailFox shows whatever `claude --version` reports |
| Config file | `%USERPROFILE%\.claude\settings.json` (user-level settings) |
| Events registered | `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `Notification`, `Stop`, `SessionEnd` (tool events use `"matcher": "*"`) |
| Hook command | `"<path>\agenttracker-hook.exe" --connector claude_code --event <Event> --at-owner agenttracker` (timeout 5 s) |

## How install works
1. **Connections → Show what will change** shows the exact line diff before anything is written.
2. **Connect** adds only TrailFox's entries and keeps a one-time backup of your original file.
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
- If TrailFox isn't running, events go to `~/.agenttracker/fallback_queue.jsonl` (redacted)
  and are replayed on next start.
- Event-to-task matching uses the agent's working folder.

## How Claude Code is detected

1. `claude --version` on the PATH the TrailFox window sees.
2. If that fails: the usual install folders — `%USERPROFILE%\.local\bin\claude.exe` (native installer),
   `%USERPROFILE%\.claude\local\`, `%APPDATA%\npm\claude.cmd`, `%LOCALAPPDATA%\Microsoft\WinGet\Links\claude.exe`.
3. The copy of Claude Code that ships with the Claude desktop app (Code tab). The newest version folder is tried first
   (version folders are compared by number, so `2.1.10` comes before `2.1.9`):
   - Microsoft Store / MSIX install:
     `%LOCALAPPDATA%\Packages\Claude_*\LocalCache\Roaming\Claude\claude-code\<version>\claude.exe`
     (the package id after `Claude_` is often `pzs8sxrjxfjjc`, but any `Claude_*` folder is checked)
   - Regular install: `%APPDATA%\Claude\claude-code\<version>\claude.exe`

   If that built-in `claude.exe` doesn't answer `--version`, it still counts as found and the card shows
   "Claude desktop app (built-in Claude Code)".
4. If no `claude.exe` is found at all, the existence of any of these still counts as "found (built-in Claude Code)":
   a `%LOCALAPPDATA%\Packages\Claude_*` folder, `%APPDATA%\Claude` (when it contains a `claude-code` folder),
   `%LOCALAPPDATA%\Claude`, `%LOCALAPPDATA%\AnthropicClaude`, `%LOCALAPPDATA%\Programs\Claude`, or the
   Claude Code VS Code/Cursor extension.

The card's notes say in plain words where Claude Code was found. If nothing is found but hooks are installed,
the card explains that the hooks will start working once Claude Code runs, and suggests starting a Claude session.

Hooks in `~/.claude/settings.json` are documented to apply to the desktop app too, but users have reported them
not firing there (anthropics/claude-code#30953). Not verified by us either way. The `claude` command in a terminal is
the most reliable way to get events.
