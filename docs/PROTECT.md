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
protect-engine serve                          # start the engine
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
- **In the app.** The runtime serves `GET /api/ui/runtime/protect/status`, `POST .../pair`,
  `GET .../grants` and `POST .../sandbox-check`, and there is a **Houndbit Protect** panel
  (`static/protect.js`) showing whether the engine is running and paired, a pair button, the folder
  list, and a per-folder sandbox check.
- **What was checked (on Linux):** `crates/agenttracker-runtime/tests/protect_connector.rs` starts
  the engine, has Houndbit pair, accepts the pairing, adds a folder, lists grants, runs the sandbox
  check and confirms the original file is unchanged. The whole workspace compiles. The panel itself
  was **not** clicked through in a live window here; only the endpoints and connector were tested.

## Honest limits

- The sandbox only covers the four file actions, and only for the folders you add. It is a confined
  file worker, not operating-system isolation: it does not stop another program from touching files
  on its own, outside Protect.
- There is no Windows isolation backend yet (job objects, restricted tokens, containers). Gate and
  isolate modes stay unavailable.
- The pipe uses Windows' default permissions. They haven't been tightened yet.
- The command line is not a trusted screen. Any program running as you could use it to approve
  things. The real approval window has to come before gate mode is switched on.
- Anything harmful already running as your Windows user is out of scope.
- **Claude Code note for later:** a Claude Code hook only blocks when it exits with code 2 (or
  replies "deny"). If it crashes or takes too long, Claude Code carries on. A gate hook therefore
  has to keep a short timer of its own and exit 2 whenever the engine can't be reached.

## Test results

- Linux (this build machine): the rule tests, the end-to-end protocol tests (including the sandbox
  staging an action and never touching the originals), the worker unit tests, the Houndbit plug-in
  test and the Houndbit connector test all passed. See the full run in the pull request.
- Windows: these run in the GitHub build (`cargo test --workspace`). See that run for the result.
  Until it passes, the named-pipe version counts as untested.

## Milestones from the spec

| Part | What it is | Status |
|---|---|---|
| A | Engine, pairing, records, standalone window | Engine done and tested. **Window not built** (command line only). JSON schemas not written yet. |
| B | Restricted worker that actually does the file work | Done for the four file actions and tested on Linux (snapshot + writing area, originals never touched). A confined file worker, **not** OS isolation; no Windows isolation backend yet. Not run on Windows named pipes. |
| C | Python and TypeScript kits plus a sample app | Small Python and Node.js examples work on Linux. Not full kits yet, and not run on Windows. |
| D | Houndbit talking to Protect through the same public door | Done and tested on Linux: connector, HTTP endpoints and a panel in the app. The panel was **not** clicked through in a live window here. |
| E | MCP gateway and Claude Code gate hook | Not started. Needs a live check on your PC. |
| F | Payments, license, signed installer | Not started. Needs your choice of payment service, a code-signing certificate and a server. |
| G | More hosts and launchers | Not started. |
