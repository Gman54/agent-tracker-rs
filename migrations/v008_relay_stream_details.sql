-- v008: Chat recorder stream details, so an unfinished reply can be told apart from a real error.
-- New columns are NULL on recordings made before this; the app falls back to "cause unknown".
ALTER TABLE relay_calls ADD COLUMN end_marker INTEGER;          -- 1 = the model server said the reply was finished ([DONE], finish_reason, Ollama done:true)
ALTER TABLE relay_calls ADD COLUMN downstream_closed INTEGER;   -- 1 = the recorder saw the chat app close its connection before the reply ended
ALTER TABLE relay_calls ADD COLUMN stream_error TEXT;           -- the connection to the model server broke mid-reply (read error)
ALTER TABLE relay_calls ADD COLUMN first_chunk_at INTEGER;      -- ms timestamps of the first and last piece received from the model server
ALTER TABLE relay_calls ADD COLUMN last_chunk_at INTEGER;
ALTER TABLE relay_calls ADD COLUMN chunk_count INTEGER;         -- pieces (network reads) received from the model server
ALTER TABLE relay_calls ADD COLUMN bytes_received INTEGER;      -- reply bytes received from the model server
ALTER TABLE relay_calls ADD COLUMN capture_cut INTEGER;         -- 1 = the reply was bigger than the recorder keeps (16 MB); the rest was passed on but not saved
