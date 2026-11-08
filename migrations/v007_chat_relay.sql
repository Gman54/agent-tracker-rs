-- v007: Chat recorder. A local relay between a chat app and the user's own model server
-- (LM Studio, Ollama, ...). Each call the chat app makes through it is recorded here, redacted.
-- Rows can be deleted by the user (no append-only trigger); each row's hashes go on the chain.
CREATE TABLE relay_calls (
    id TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL,      -- best guess: same first system + first user message
    title TEXT,
    api TEXT NOT NULL,                  -- chat_completions | completions | responses | ollama_chat | ollama_generate
    path TEXT NOT NULL,
    client TEXT,                        -- User-Agent the chat app sent, if any
    model TEXT,
    streamed INTEGER NOT NULL DEFAULT 0,
    message_count INTEGER,
    last_user_redacted TEXT,
    request_redacted TEXT,
    reply_redacted TEXT,
    reasoning_redacted TEXT,
    reasoning_source TEXT,              -- reasoning_content | reasoning | think_tags | thinking | reasoning_items
    tool_calls TEXT,
    finish_reason TEXT,
    usage TEXT,
    http_status INTEGER,
    error TEXT,
    request_sha256 TEXT NOT NULL,
    response_sha256 TEXT,
    started_at INTEGER NOT NULL,
    finished_at INTEGER
);
CREATE INDEX relay_calls_conv ON relay_calls(conversation_id, started_at);
CREATE INDEX relay_calls_started ON relay_calls(started_at);
