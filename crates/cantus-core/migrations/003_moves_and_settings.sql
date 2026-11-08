ALTER TABLE journal ADD COLUMN new_path TEXT;

CREATE TABLE setting (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
