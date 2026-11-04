-- v002: saved check templates ("Saved Checks" screen)
CREATE TABLE check_templates (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    check_type TEXT NOT NULL,
    params TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    source TEXT NOT NULL DEFAULT 'user'
);
