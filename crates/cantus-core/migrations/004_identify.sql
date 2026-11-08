ALTER TABLE track ADD COLUMN recording_mbid TEXT;
ALTER TABLE track ADD COLUMN release_mbid TEXT;

CREATE TABLE lookup_cache (
    key TEXT PRIMARY KEY,
    body TEXT NOT NULL,
    fetched_at INTEGER NOT NULL
);
