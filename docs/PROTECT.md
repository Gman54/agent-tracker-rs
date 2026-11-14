# Houndbit Protect: where things stand

Short version: Protect now **carries out the file work it allows, inside a sandbox.** When an app
is allowed to list, read, write or compare files, Protect does that work on a read-only snapshot
plus a separate writing area, so the real files are never touched. Houndbit itself now talks to
Protect through the same public door, with a panel in the app.

Be clear about what this is and isn't: the sandbox is a **confined file worker**. It runs only the
four file actions below, each one checked against your rules and kept inside the snapshot and
writing area. It is **not** operating-system isolation: it does not sandbox another program's own
file or shell access, and there is no Windows isolation backend yet. Gate and isolate modes are
still reported as unavailable.

## The Protect add-on switch

Protect is meant to become a paid add-on later. For now it is **on for testing**:

- The Protect page shows "Protect add-on: on for testing" with an on/off switch. Off shows
  "Protect add-on is off", greys out the folder guard and stops it from blocking anything (the
  hooks and the chat recorder let everything through).
- The switch is the `"enabled"` field in `guard.json`, so the hook program sees it even when the
  Houndbit window is closed. A missing field means on, so older lists keep working.
- Endpoint: `POST /api/ui/runtime/protect/addon {"enabled": true|false}`; `GET .../protect/guard`
  reports `addon_enabled`.
- **Where a license check would go:** `tracker_common::guard::addon_active` (read by the hook, the
  relay and the page) and `protect::set_addon` (switching it on). Both are marked
  `FUTURE LICENSE CHECK`. No license check exists today; part F (payments, license server) needs
  your payment service, certificate and server.
- The switch only covers the folder guard. Houndbit's safe copy (parts B and D) is not affected.

## Folder guard for AI helpers (part E, first pieces)

Before the guard, Protect did **not** watch Claude Code at all. A test where Claude Code wrote
primes into `F:\Test\sigmadelta.txt` went straight through.

How it works:

- On the Protect page you add folders and pick **Always block** or **Ask me first**. The list is
  saved to `%USERPROFILE%\.agenttracker\guard.json`.
- Houndbit's hook program (`agenttracker-hook`) reads that list itself, so it works even when the
  Houndbit window is closed.
- What counts as "would change":
  - file tools (Write, Edit, MultiEdit, NotebookEdit, Delete and any other non-read tool) whose
    paths are inside the folder;
  - any command (Bash, PowerShell, Cursor's Shell) whose text names the folder, including Git Bash
    `/f/...` and WSL `/mnt/f/...` spellings;
  - any command whose working folder is inside the guarded folder;
  - **new:** any command that runs a script file (`.py`, `.js`, `.ps1`, `.sh`, `.bat`, `.cmd` and
    similar, or the file after `-File`) that is stored in the guarded folder, or whose text names
    the guarded folder. Only scripts up to 256 KB are read (`SCRIPT_READ_CAP`); bigger ones are only
    checked for where they are stored.
  - Read, Grep, Glob and MCP tools whose names start with read/list/get/search/find/view/fetch stay
    allowed.
- **Ask me first** shows a request on the Protect page (and a pop-up note on other pages) naming
  the app that asked. The hook waits up to 90 seconds; no answer means stopped.
- If `guard.json` exists but can't be read, every non-read tool is stopped until it's fixed.
- Every decision is listed under "What Protect stopped or let through", with the app's name.

### Claude Code

The hook runs on every `PreToolUse`. A stop is exit code 2 plus a plain reason, which stops the
tool and tells Claude why. Claude Code's timeout for this hook is 120 seconds so the guard's own
90-second wait decides. Older installs with a 5-second timeout are updated when Houndbit starts.
Whether the Claude desktop app's Code tab uses the same settings file has to be checked on your PC.

### Cursor (new, untried with a live Cursor)

Connecting Cursor on the Connections page now also installs Cursor's three hooks that can say no:
`preToolUse` (every agent tool, including Shell, Write, Delete and `MCP:<tool>`),
`beforeShellExecution` and `beforeMCPExecution`, each with a 120-second timeout. Older Cursor
installs are updated when Houndbit starts.

- The hook always answers these with Cursor's JSON: `{"permission":"allow"}`, or
  `{"permission":"deny","user_message":...,"agent_message":...}` with the reason. (Cursor treats
  an empty answer as a block, so the hook never stays silent on these.)
- **Ask me first** asks in Houndbit, not in Cursor's own window. Cursor does not honour "ask" on
  `preToolUse`, so Houndbit waits for your answer itself and then says allow or deny. Cursor runs
  both `preToolUse` and `beforeShellExecution` for one command; an OK given for the first covers the
  second for two minutes, so you are only asked once.
- Cursor's `beforeReadFile` is not used (reading stays allowed) and `afterFileEdit` can't block.
- Cursor's own file-editing path: Cursor's docs list Write and Delete as `preToolUse` tools, but
  the exact fields in their requests aren't documented. The guard looks at every path-like field,
  so it should catch them, but this has only been tested with requests shaped from the docs.
- If the hook program crashes, Cursor carries on (Cursor fails open unless `failClosed` is set;
  Houndbit doesn't set it, so a broken hook can't lock you out of Cursor).
- Tested on Linux by running the real hook program with Cursor-shaped requests
  (`crates/agenttracker-hook/tests/guard.rs`). **Not tried with a live Cursor on Windows.**

### Chat recorder (new)

When a chat app sends a request through Houndbit's chat recorder and offers the model tools, and
folders are guarded, the recorder holds the model's reply until it is complete (so that reply is
not streamed live) and checks every tool call in it. If one would change a guarded folder:

- the chat app gets a short refusal instead, in the same format (plain JSON or a stream, for
  OpenAI chat/completions, the Responses API and Ollama), with no tool call in it;
- it is written to the guard log as "Chat recorder";
- **Ask me first folders are blocked too**: a reply can't wait halfway for your answer, so the
  recorder stops it and says so in the log.

The recorded call still shows what the model really asked for. Replies over 16 MB are passed
through unchecked. Requests without tools aren't held or checked. Tested on Linux with a fake model
server (`crates/agenttracker-runtime/tests/relay_guard.rs`); **not tried with LM Studio, Ollama or
a real chat app.** The Responses API streaming refusal in particular is untried with a real client.

### MCP gateway: not built

Spec part E also describes an MCP gateway (Houndbit sitting between an AI app and its MCP servers,
holding their credentials and checking every call). It needs an MCP stdio shim, upstream proxying
and credential storage in the engine. That is too big to do solidly with tests in this round, so
it was **skipped**. MCP tool calls are still covered by the hooks above (Claude Code's
`mcp__server__tool` and Cursor's `beforeMCPExecution`), but only by path matching.

### Limits, plainly

This matches paths named in the request, the working folder, and small scripts. A command can
still reach a folder in ways it can't see: a folder name built while the command runs, a program
that isn't a script, a big script, or a script that fetches another one. That is the spec's point
that string matching isn't real confinement. If the hook program itself crashes or is removed,
Claude Code and Cursor carry on.

## What works today (parts A, B and D)

The engine is `crates/protect-engine`. It is a separate program from Houndbit, keeps its own
records, and runs fine without Houndbit installed.

- **Its own records.** It uses its own database in `%LOCALAPPDATA%\HoundbitProtect`. Every pairing,
  session, request, decision and approval is saved there. If a save fails, the request is refused.
- **Pairing.** An app asks to pair and the person at the computer accepts or rejects it. After that
  the app proves who it is with a key it holds. Apps can't give themselves approval rights.
- **Folders.** An app only gets the folders you add for it. Paths that try to leave the folder
  (`..`, other drives, network paths, odd Windows names) are refused.
- **Rules.** A built-in starting rule set covers listing, reading, writing a staged copy and showing
  differences. Secret-looking files (`.env`, `.git`, keys) are always refused. Each rule can be set
  to allow, ask or deny.
- **Decisions.** Every request gets a decision: allowed, refused, or waiting for you.
- **The sandbox does the work (part B).** When a request is allowed, Protect carries it out in a
  sandbox: it makes a read-only snapshot of the folder, keeps a separate writing area, and runs the
  one allowed action there. **List** names the files it can see, **read** returns a file's contents,
  **write** saves into the writing area only, and **diff** shows what changed. Writes never touch
  the real files; the record shows `wrote_original: false`. Symlinks and secret-looking files are
  left out of the snapshot. File-count and size caps keep a snapshot from growing without bound.
  Checked on Linux (`crates/protect-engine/src/worker.rs` unit tests and the end-to-end protocol
  tests).
- **Approvals.** Each approval works once, only for the exact request it was made for, and runs out
  after 15 minutes. Once approved, the allowed action is carried out in the same sandbox.
- **Connection.** On Windows apps connect through a local named pipe; remote machines are turned
  away. Messages over 1 MB are refused, and so are repeat requests and requests from apps that
  haven't signed in.

What the engine tells apps about itself: it carries out the four staged file actions, and watch
mode is on. Gate and isolate are reported as **unavailable**.

## Trying it (command line for now; there's no window yet)

```
protect-engine serve                          # optional: Houndbit starts Protect for you
protect-engine pairings                       # see who wants to pair
protect-engine pairings accept <id>           # accept one
protect-engine folders add <app> <folder>     # let an app use a folder
protect-engine policy set files.write_staged ask
protect-engine approvals                      # see what is waiting
protect-engine approvals approve <id>
```

## Plugging in: Houndbit and other programs

Protect has one door, and every app uses it the same way: Houndbit, your own scripts, or
someone else's tool. No app gets a private shortcut.

1. **Pair once.** The app sends its name and a public key. You accept it (for now with
   `protect-engine pairings accept <id>`). The app keeps its key in its own folder, so it stays
   paired after a restart.
2. **Sign in.** Each time it connects, the app proves it holds that key.
3. **Find its folder.** `v1.grants.list` shows the app the folders you gave it, and only those.
4. **Ask and read back.** It starts a session, asks to do something, and reads the record.

What the message format looks like: a 4-byte length (big-endian), then JSON-RPC 2.0 text. The
sign-in message is `houndbit-protect-auth-v1`, the engine id and the nonce, one per line, signed
with Ed25519. Key files are the 32-byte secret in base64, the same in every language.

Ready-made pieces:

- **Rust (Houndbit itself):** `protect_engine::client` with `Client`, `sign_in` and
  `load_or_create_key`.
- **Python:** `examples/protect-clients/python/protect_client.py` (needs `pip install cryptography`).
- **Node.js:** `examples/protect-clients/node/protect_client.mjs` (no packages needed).
- **Check it all at once (Linux/macOS):** `examples/protect-clients/try_it.sh`.

Running alongside other software: Protect doesn't open a network port, so it can't clash with
Houndbit's port or anyone else's. It keeps its own database, separate from Houndbit's. Only one
engine runs per Windows user; a second copy refuses to start. If Protect isn't running, apps get
a plain "couldn't connect" error, never a silent yes, and Houndbit carries on with its own checks.

What was checked (on Linux; Windows below):
- `tests/tests/protect_plugin.rs`: Houndbit pairs as an ordinary app, keeps its key across a
  restart, finds its folder, asks to read a file and reads the contents back. It runs a normal
  Houndbit check at the same time. Neither database contains the other's tables. With Protect
  stopped, connecting fails and Houndbit's own check still passes.
- `try_it.sh` ran the real `protect-engine` program with the Python and Node.js examples. Both
  paired, signed in, found their folder and ran a file action in the sandbox. Running again with
  the saved key signed in without pairing again.
- Not checked yet: the Python and Node.js examples on Windows named pipes. The code paths for
  Windows are there but have not been run.

## Houndbit's own panel (part D)

Houndbit now talks to Protect as an ordinary paired app, through the same door every other app
uses. No private shortcut.

- **Connector.** `crates/agenttracker-runtime/src/protect.rs` holds Houndbit's own key, pairs,
  signs in, lists its folders, and runs a sandbox check (read a file, write a staged copy, list,
  diff) to confirm the real file is never touched.
- **Protect starts with Houndbit.** When Houndbit opens, it starts the Protect engine itself in the
  background on Protect's normal endpoint (`protect::autostart`). One install, nothing extra to run.
  If a separate `protect-engine serve` is already running for this user, Houndbit leaves it alone
  and connects to it like any other app. When Houndbit quits, its built-in Protect stops too.
- **In the app.** The sidebar has a **Protect** page (`static/protect.js`): shows Protect is on
  and that Houndbit is linked to it automatically (there is no Connect button), a box to add folders, a **Try it** sandbox check per
  folder, a "Waiting for your OK" list (other apps asking to connect, actions needing approval) with
  Allow / Say no, and a list of other connected apps with Add folder / Disconnect. Endpoints:
  `GET .../protect/status`, `GET .../protect/grants`,
  `POST .../protect/sandbox-check`, and `GET .../protect/admin` plus
  `POST .../protect/admin/{pairing,folder,remove-app,approval}`.
- **Houndbit links itself.** When Houndbit hosts Protect (the normal case), it pairs with its own
  built-in engine by itself the first time the Protect status is asked for (or when it first needs
  Protect), and accepts that pairing straight away with no question: it is your own app talking to
  its own built-in engine (`protect::ensure_linked`). If the engine knows Houndbit by an older key
  (for example after Houndbit's data folder was reset), Houndbit replaces its own old entry; the
  folders given to the old entry are not carried over. With a separate `protect-engine serve`,
  Houndbit sends it one pairing request per run and you accept it there, like any other app. Other
  apps' pairing requests are unchanged: they still wait for your Allow on the Protect page.
- **The Protect controls only act on Houndbit's built-in engine.** Houndbit's file work still goes
  through the normal signed-in client path with its own key, like any other app.
- **What was checked (on Linux):** `tests/protect_connector.rs` (separate engine: pair, accept,
  folder, sandbox, original unchanged) and `tests/protect_hosted.rs` (built-in engine: starts,
  links itself with nothing waiting for an OK, links again after its entry is removed, replaces an
  old key, another app's request shows up and can be refused, add folder, sandbox,
  original unchanged, disconnect). The Protect page was also clicked through in a browser using the
  headless preview (`cargo run -p agenttracker-runtime --example ui_preview`); the latest check
  confirmed it shows "Houndbit is linked to Protect automatically" with no Connect button, and that
  the add-on switch turns off and saves `"enabled": false`. Not yet clicked through in the Windows app.

## Local model helper (spec section 12, new, untried with a real model)

The Protect page has a new card under Houndbit's folders: **"Let a model on this PC work on a folder."** You pick a model,
pick one of Houndbit's folders, type what you want done, and press Start. The model works only
through Protect's sandbox, and nothing reaches your real folder until you look at each change and
press Apply.

How to use it:

1. On the **Runtime** page, set the model server (LM Studio, Ollama or any OpenAI-style server on
   this PC). The helper uses that same address.
2. On the **Protect** page, press **Find models**, pick one, pick a folder, type your request.
3. Press **Start**. The step list shows what the model is doing. **Stop** ends it early.
4. When it's done, each changed file shows **Now** and **After**. Press **Apply** or **Don't apply**
   per file, or **Apply all** / **Don't apply any**.

What the model can and can't do:

- It can only ask for four things: list files, read a file, write a changed copy (into Protect's
  writing area, never the real folder), or say it's done. No shell, no running code, no internet,
  no deleting files.
- Every request goes through Protect's normal rules: the "never touch" list (`.env`, keys and so on)
  still applies, and anything that needs your OK waits in "Waiting for your OK".
- A file that was only partly read (too big) can't be rewritten.
- It stops after 30 steps, or after 3 replies it can't understand.
- **Apply** writes to the real file only if the original hasn't changed since the snapshot was
  taken. If you or another program edited it meanwhile, Apply refuses and says so. Applies,
  refusals and discards are recorded as `changes.applied`, `changes.refused` and
  `changes.discarded`.
- Only a model on this PC (localhost) runs without asking. Any other address needs you to tick a
  box confirming file contents will leave your PC.
- A model's answer never grants permission. Only you do, on the page.

Limits, plainly:

- Same sandbox as above: a confined file worker, not operating-system isolation.
- Houndbit doesn't judge whether the model's work is any good. That's what the review is for.
- Jobs are kept in memory. If Houndbit restarts, the job list is gone; staged copies stay in
  Protect's data folder until discarded.
- Endpoints: `GET .../protect/model`, `POST .../protect/model/run`, `GET .../protect/model/job/:id`,
  `POST .../protect/model/{stop,apply,discard}`.
- **What was checked (on Linux):** `tests/protect_model.rs` with a scripted stand-in model (not a
  real one): the `.env` read is refused, nothing is written before Apply, applying one file works,
  a file edited by hand blocks Apply, discarding the rest finishes the job, and a model that never
  answers properly ends as failed. The page was also clicked through in the headless preview
  against a small fake model server: Find models, Start, the Now/After review, Apply, and the real
  file changed only after Apply. Not yet tried with LM Studio or Ollama, and not on Windows.

## Start an AI tool in a locked job (new, Windows only, compile-checked not run)

Houndbit can start a program (for example a coding AI, or `powershell`) inside a locked Windows
*job object*. A job is a Windows container for one or more processes. We use three levers:

- **Kill on close** (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`): the whole tree is killed the moment
  Houndbit (or the job handle) goes away. Nothing is left running behind your back.
- **Memory cap** (optional, `JOB_OBJECT_LIMIT_PROCESS_MEMORY`): a ceiling in megabytes.
- **No other programs** (optional, strict): the tool is launched with
  `PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY = PROCESS_CREATION_CHILD_PROCESS_RESTRICTED`, which
  stops it from starting *any* child process at all. This is the real "it cannot run other programs"
  switch.

How it is wired: a fresh job (`CreateJobObjectW`) gets its limits (`SetInformationJobObject`); the
program is created suspended with `CreateProcessW` (plus `STARTUPINFOEXW` and an attribute list when
the strict switch is on), put into the job (`AssignProcessToJobObject`), then resumed. Houndbit keeps
the job and process handles so it can stop the whole tree later (`TerminateJobObject`).

- Endpoints: `GET /api/ui/runtime/protect/launch` (what's running + whether this build supports it),
  `POST .../protect/launch` (start: program, args, working_dir, memory_mb, block_child_processes),
  `POST .../protect/launch/stop` (stop by pid). UI: a card on the Protect page.

Honest limits for this piece:

- **Windows only.** The Win32 code is compiled and checked on the Linux build machine by
  cross-compiling to `x86_64-pc-windows-gnu`, and it compiles clean. It has **not** been run on a
  real Windows box from here. On non-Windows builds the launch call returns a plain "Windows-only"
  error and the card says so.
- It only covers tools **Houndbit launches itself**. A tool you opened from the Start menu, or one
  already running, is not inside the job and is not affected.
- The strict "no other programs" switch is blunt. Some AI CLIs legitimately shell out to `git`,
  `python`, and so on; turning it on will break those. It is off by default.
- The strongest coverage is still a limited or separate Windows account for the AI tool. The locked
  job narrows what one launched tool can do; it is not a whole-machine fence.

## Screen-clicking AIs (what a locked job does *not* stop)

Some AI tools don't run commands at all — they move the mouse and click the screen like a person
(Claude's "Cowork"/computer-use, ChatGPT's computer use, and similar). Houndbit's folder guard and
the locked job can't catch these, because no tool hook fires and no new program is launched: they
drive apps that are already open. A locked job only fences a program Houndbit *starts*, so it does
nothing against a screen-clicker driving your existing browser or editor.

What actually helps, roughly strongest first:

- **Run the screen-clicking AI under a separate, limited Windows account** (or in Windows Sandbox).
  It can then only see and click that account's apps and files, not your main ones. This is the one
  real fence.
- **Use the tool's own per-app permissions.** Claude's computer use blocks finance/crypto sites by
  default and asks before acting; ChatGPT's computer use asks per app/site. Keep those prompts on and
  add your own blocklist.
- **AppLocker / Windows app control** can stop *programs* from starting, but only on Windows 10/11
  Pro or Enterprise, and it won't stop clicks inside an app that's allowed to run.
- Keep the things you care about (wallets, banking, crypto tools) in that separate account, so a
  screen-clicker in your everyday account simply can't see them.

## Honest limits

- The sandbox only covers the four file actions, and only for the folders you add. It is a confined
  file worker, not operating-system isolation: it does not stop another program from touching files
  on its own, outside Protect.
- The *sandbox worker* still has no Windows isolation backend (restricted tokens, containers). Gate
  and isolate modes stay unavailable. The locked-job launcher above does use job objects, but that is
  a separate feature for tools Houndbit starts, not isolation for the sandbox worker.
- The pipe uses Windows' default permissions. They haven't been tightened yet.
- The command line is not a trusted screen. Any program running as you could use it to approve
  things. The real approval window has to come before gate mode is switched on.
- Anything harmful already running as your Windows user is out of scope.
- **Claude Code and Cursor:** a hook only blocks when it says so (exit code 2 for Claude Code,
  `"deny"` for Cursor). If it crashes or takes too long, the app carries on. The folder guard keeps its own 90-second timer under
  Claude's 120-second limit and stops the change when nobody answers.

## Test results

- Linux (this build machine): the rule tests, the end-to-end protocol tests (including the sandbox
  staging an action and never touching the originals), the worker unit tests, the Houndbit plug-in
  test and the Houndbit connector test all passed. See the full run in the pull request.
- Windows: these run in the GitHub build (`cargo test --workspace`). See that run for the result.
  Until it passes, the named-pipe version counts as untested.
- Locked-job launcher: its non-Windows logic (spec validation, the Windows-only guard, and the
  running-job registry list/stop bookkeeping) is unit-tested and passes on Linux. The Win32 launch
  path itself is only compile-checked by cross-compiling to `x86_64-pc-windows-gnu`; it has not been
  run on Windows from here.

## Milestones from the spec

| Part | What it is | Status |
|---|---|---|
| A | Engine, pairing, records, standalone window | Engine done and tested. **Window not built** (command line only). JSON schemas not written yet. |
| B | Restricted worker that actually does the file work | Done for the four file actions and tested on Linux (snapshot + writing area, originals never touched). A confined file worker, **not** OS isolation; no Windows isolation backend yet. Not run on Windows named pipes. |
| C | Python and TypeScript kits plus a sample app | Small Python and Node.js examples work on Linux. Not full kits yet, and not run on Windows. |
| D | Houndbit talking to Protect through the same public door | Done and tested on Linux. Protect now starts inside Houndbit; the Protect page was clicked through in the headless preview, not yet in the Windows app. |
| E | MCP gateway and Claude Code gate hook | Folder guard for Claude Code and Cursor (hooks), plus the chat recorder, with script and working-folder checks and an add-on switch. Tested on Linux with the real hook program and a fake model server; not yet tried with live Claude Code, Cursor or a chat app on Windows. Uses path matching, not OS isolation. **MCP gateway skipped** (too big to do solidly this round). |
| F | Payments, license, signed installer | Not started. An on/off add-on switch is in place (on for testing), with a marked spot for a license check. Needs your choice of payment service, a code-signing certificate and a server. |
| G | More hosts and launchers | Started: a locked-job launcher starts an AI tool inside a Windows job object (kill-on-close, optional memory cap, optional no-child-processes). Non-Windows logic tested on Linux; the Win32 path is compile-checked via cross-compile, not run on Windows. Covers only tools Houndbit starts. |
| 12 | Optional model relay | Local model helper on the Protect page: works through the sandbox, you review and Apply each change. Tested on Linux with a scripted stand-in model; not tried with a real model or on Windows. |
