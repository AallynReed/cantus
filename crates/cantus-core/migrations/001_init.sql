CREATE TABLE root (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE
);

CREATE TABLE track (
    id INTEGER PRIMARY KEY,
    root_id INTEGER NOT NULL REFERENCES root(id) ON DELETE CASCADE,
    path TEXT NOT NULL UNIQUE,
    size INTEGER NOT NULL,
    mtime INTEGER NOT NULL,
    quick_key BLOB NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('ok', 'unreadable', 'missing')),
    error TEXT,
    duration_ms INTEGER,
    bitrate INTEGER,
    sample_rate INTEGER,
    channels INTEGER,
    title TEXT,
    artist TEXT,
    album_artist TEXT,
    album TEXT,
    track_no INTEGER,
    disc_no INTEGER,
    year INTEGER,
    genre TEXT
);

CREATE INDEX track_root ON track(root_id);
CREATE INDEX track_quick_key ON track(quick_key);
