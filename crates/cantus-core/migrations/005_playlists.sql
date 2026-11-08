CREATE TABLE playlist (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL
);

CREATE TABLE playlist_entry (
    playlist_id INTEGER NOT NULL REFERENCES playlist(id) ON DELETE CASCADE,
    pos INTEGER NOT NULL,
    track_id INTEGER REFERENCES track(id) ON DELETE SET NULL,
    path TEXT,
    recording_mbid TEXT,
    artist TEXT,
    title TEXT,
    duration_ms INTEGER,
    PRIMARY KEY (playlist_id, pos)
);

CREATE INDEX playlist_entry_track ON playlist_entry(track_id);
