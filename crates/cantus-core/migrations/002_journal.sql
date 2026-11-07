CREATE TABLE change_set (
    id INTEGER PRIMARY KEY,
    created_at INTEGER NOT NULL,
    description TEXT NOT NULL
);

-- One row per file rewrite. Undo rebuilds the old file from the current one:
-- current[..prefix_len] + old_middle + current[len - suffix_len..].
CREATE TABLE journal (
    id INTEGER PRIMARY KEY,
    change_set_id INTEGER NOT NULL REFERENCES change_set(id) ON DELETE CASCADE,
    track_id INTEGER REFERENCES track(id) ON DELETE SET NULL,
    path TEXT NOT NULL,
    step TEXT NOT NULL
        CHECK (step IN ('planned', 'applying', 'done', 'undoing', 'undone', 'failed')),
    error TEXT,
    hash_before BLOB NOT NULL,
    hash_after BLOB,
    prefix_len INTEGER,
    suffix_len INTEGER,
    old_middle BLOB
);

CREATE INDEX journal_change_set ON journal(change_set_id);
CREATE INDEX journal_step ON journal(step);
