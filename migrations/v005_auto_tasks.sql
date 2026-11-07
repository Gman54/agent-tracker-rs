-- v005: tasks started automatically when an agent receives a prompt ("auto") vs set up by hand ("manual").
ALTER TABLE tasks ADD COLUMN origin TEXT NOT NULL DEFAULT 'manual';
