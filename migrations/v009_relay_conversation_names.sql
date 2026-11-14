-- v009: Names people give their chat recordings. Kept apart from relay_calls, keyed by the
-- conversation, so new messages in that chat keep the name. No row = the automatic name.
CREATE TABLE relay_conversation_names (
    conversation_id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    renamed_at INTEGER NOT NULL
);
