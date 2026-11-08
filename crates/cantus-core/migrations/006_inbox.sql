ALTER TABLE journal ADD COLUMN op TEXT;

CREATE TABLE inbox (
    id INTEGER PRIMARY KEY,
    origin TEXT NOT NULL,
    name TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    change_set_id INTEGER REFERENCES change_set(id) ON DELETE SET NULL
);

CREATE TABLE inbox_file (
    inbox_id INTEGER NOT NULL REFERENCES inbox(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    PRIMARY KEY (inbox_id, path)
);

CREATE TABLE torrent_seen (
    info_hash TEXT PRIMARY KEY
);
