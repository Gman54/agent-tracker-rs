# Houndbit Protect: where things stand

Short version: **Protect does not protect anything yet.** This first piece writes down what an
app asks to do and decides whether it would be allowed. Nothing it decides is carried out.

## What works today (part A, engine only)

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
- **Decisions.** Every request gets a decision: allowed, refused, or waiting for you. An allowed
  request is recorded as **"Nothing was run"**, because there is nothing yet that can run it safely.
- **Approvals.** Each approval works once, only for the exact request it was made for, and runs out
  after 15 minutes. Approving still doesn't run anything yet.
- **Connection.** On Windows apps connect through a local named pipe; remote machines are turned
  away. Messages over 1 MB are refused, and so are repeat requests and requests from apps that
  haven't signed in.

What the engine tells apps about itself: watch mode only. Gate and isolate are reported as
**unavailable**.

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

## Honest limits

- No sandbox or worker exists yet, so nothing is blocked "for real". Only decisions are made.
- The pipe uses Windows' default permissions. They haven't been tightened yet.
- The command line is not a trusted screen. Any program running as you could use it to approve
  things. The real approval window has to come before gate mode is switched on.
- Anything harmful already running as your Windows user is out of scope.
- **Claude Code note for later:** a Claude Code hook only blocks when it exits with code 2 (or
  replies "deny"). If it crashes or takes too long, Claude Code carries on. A gate hook therefore
  has to keep a short timer of its own and exit 2 whenever the engine can't be reached.

## Test results

- Linux (this build machine): 3 rule tests and 8 end-to-end connection tests passed.
- Windows: these run in the GitHub build (`cargo test --workspace`). See that run for the result.
  Until it passes, the named-pipe version counts as untested.

## Milestones from the spec

| Part | What it is | Status |
|---|---|---|
| A | Engine, pairing, records, standalone window | Engine done and tested. **Window not built** (command line only). JSON schemas not written yet. |
| B | Restricted worker that actually does the file work | Not started. Needs testing on a real Windows PC. |
| C | Python and TypeScript kits plus a sample app | Not started. |
| D | Houndbit talking to Protect through the same public door | Not started. |
| E | MCP gateway and Claude Code gate hook | Not started. Needs a live check on your PC. |
| F | Payments, license, signed installer | Not started. Needs your choice of payment service, a code-signing certificate and a server. |
| G | More hosts and launchers | Not started. |
