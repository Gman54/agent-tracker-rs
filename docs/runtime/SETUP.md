# Decision runtime: setup (Windows, PowerShell)

## 1. Get the code and run the tests

```powershell
cd F:\
git clone https://github.com/Gman54/agent-tracker-rs.git
cd agent-tracker-rs
git checkout feature/decision-runtime
cargo test -p agenttracker-runtime
```

Expected: `test result: ok. 23 passed; 0 failed`. These tests are **simulated**: they use a scripted
provider and need no model.

## 2. Install a local model server (choose one)

**Ollama** (https://ollama.com/download), then:

```powershell
ollama pull qwen2.5:7b
ollama list
```

`qwen2.5:7b` is a suggestion, not a tested model. Only `qwen2.5:1.5b` was tried, and it did not
complete the demos (see CAPABILITY_REPORT.md). LM Studio's docs warn that models under 7B may not
handle structured output. With 16 GB on your RTX 5070 Ti, a 7B–14B instruct model is a reasonable
next test.

**LM Studio** (https://lmstudio.ai): download a model, open the Developer tab and start the server.
The default endpoint is `http://127.0.0.1:1234/v1`. LM Studio has **not** been tested with this runtime.

## 3. Run the app and configure the provider

```powershell
cargo run -p agenttracker
```

In the app, open **Agent Runtime → Model provider**:

- Provider: Ollama (or LM Studio)
- Endpoint: `http://127.0.0.1:11434/v1` (Ollama) or `http://127.0.0.1:1234/v1` (LM Studio)
- Model: the exact name from `ollama list`, e.g. `qwen2.5:7b`
- Click **Test connection**. You should see the model list and, for Ollama, its digest. Then click **Save**.

Endpoints other than 127.0.0.1/localhost are refused unless you tick "Allow non-local endpoints".

## 4. Try the demonstrations

**Agent Runtime → Demonstrations**:

- **Recorded fixture**: deterministic, no model needed. Labelled "Recorded fixture" everywhere.
- **Live model**: uses your configured model in a disposable folder under
  `%USERPROFILE%\.agenttracker\runtime-demos`. Whatever the model does is recorded as-is.

Then open a session and click **Why this action?** on an action (or **Inspect decision** for a text-only response).

## 5. Command-line live run (optional)

This runs both demos against a model and prints every attempt, decision, action and resulting file:

```powershell
cargo run -p agenttracker-runtime --example live_demo -- http://127.0.0.1:11434/v1 qwen2.5:7b F:\agenttracker-live\live.db both investigate
```

Arguments: `<endpoint> <model> <db_path> [a|b|both] [investigate]`. Demo folders are created under
`%TEMP%\agenttracker-live-demos`. With `investigate`, if Demo B produced an accepted action, it
also runs a 3-trial "remove evidence_1" influence test (decision stage only, no tools).

## 6. Your own task

**Agent Runtime → Start a task**: write the instruction, choose a working folder (use a copy, not
your only copy of anything), list protected paths (one per line, relative to the folder) and add any
documents (one relative path per line; they are recorded as lower-trust retrieved content). `write_text_file` needs your approval by default.

## Data and privacy

- Records are stored in the existing TrailFox SQLite database (`rt_*` tables).
- Audit copies of requests are redacted. Exact payloads are kept only if you enable
  **Encrypted replay capture**. Delete them with **Delete all replay captures**.
- Nothing leaves your computer unless you point the endpoint at a remote server and allow it.
