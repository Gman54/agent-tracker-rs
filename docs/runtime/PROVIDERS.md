# Model providers

The runtime talks to a **local OpenAI-compatible** endpoint (`POST <endpoint>/chat/completions`,
`GET <endpoint>/models`). It does not assume that Ollama and LM Studio behave the same way. For
every request it records which structured-output mode the server accepted
(`rt_model_requests.structured_output_mode`) and whether the server exposed any reasoning text.

Docs checked on 2026-11-04:

- Ollama OpenAI compatibility: https://docs.ollama.com/api/openai-compatibility
- LM Studio structured output: https://lmstudio.ai/docs/developer/openai-compat/structured-output
- LM Studio REST endpoints (model metadata): https://lmstudio.ai/docs/developer/rest/endpoints

## Differences that matter here

| | Ollama | LM Studio |
|---|---|---|
| Default endpoint | `http://127.0.0.1:11434/v1` | `http://127.0.0.1:1234/v1` |
| `response_format` | Listed as supported ("JSON mode" plus `response_format`) | `json_schema` documented. `json_object` is not mentioned in the structured-output page |
| Model digest | `GET /api/tags` returns `digest` per model; the runtime records it | No digest. `GET /api/v0/models` gives arch/quantization/context length, which are recorded as details; the digest is recorded as unavailable |
| Reasoning text | Depends on the model; may appear as `reasoning`/`reasoning_content` | Depends on the model |
| Small models | Not stated on the compatibility page | Docs warn models below 7B parameters may not be capable of structured output |
| `seed` | Listed as a supported field | Not guaranteed to make output reproducible |

The runtime does **not** use provider-native tool calling. Tools are described in the prompt, and
the model returns them inside the decision proposal. This keeps one validated path to execution on
both servers.

## Structured-output modes

`structured_mode` setting:

- `auto` (default): tries `json_schema`, then `json_object`, then `prompt_only`, and remembers the first mode the server accepts.
- `json_schema`, `json_object`, `prompt_only`: forces that mode.

Whatever the mode, the reply is validated by the runtime. A server accepting a schema does not
guarantee the content is valid, so validation failures are stored and repaired within the retry limit.

## What is recorded per request

`provider`, `endpoint`, `model`, `model_digest` (or null), `settings` (temperature, top_p, seed,
max_tokens, context limit, timeout, structured mode; auth is recorded only as
`"bearer (value not recorded)"` or `"none"`, never the key), `runtime_version`, `structured_output_mode`, status and timing.

## Support status

A model is only called supported after a decision proposal **and** a tool execution have both been
observed with it. See [CAPABILITY_REPORT.md](CAPABILITY_REPORT.md). As of this commit, **no model is
marked supported**.
