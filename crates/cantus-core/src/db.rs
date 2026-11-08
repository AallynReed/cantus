use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Row};

use crate::{Metadata, Result};

const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/001_init.sql"),
    include_str!("../migrations/002_journal.sql"),
    include_str!("../migrations/003_moves_and_settings.sql"),
];

pub(crate) const TRACK_COLUMNS: &str = "id, path, state, error, duration_ms, bitrate, sample_rate, \
     channels, title, artist, album_artist, album, track_no, disc_no, year, genre";

/// The music library database.
pub struct Library {
    pub(crate) conn: Connection,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackState {
    Ok,
    Unreadable,
    Missing,
}

impl TrackState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Unreadable => "unreadable",
            Self::Missing => "missing",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "unreadable" => Self::Unreadable,
            "missing" => Self::Missing,
            _ => Self::Ok,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Track {
    pub id: i64,
    pub path: PathBuf,
    pub state: TrackState,
    pub error: Option<String>,
    pub meta: Metadata,
}

impl Library {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.busy_timeout(Duration::from_secs(5))?;

        let version: u32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        let tx = conn.transaction()?;
        for (sql, v) in MIGRATIONS.iter().zip(1u32..).skip(version as usize) {
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", v)?;
        }
        tx.commit()?;
        Ok(Self { conn })
    }

    pub fn roots(&self) -> Result<Vec<PathBuf>> {
        let mut stmt = self.conn.prepare("SELECT path FROM root ORDER BY path")?;
        let roots = stmt.query_map([], |r| r.get::<_, String>(0).map(PathBuf::from))?;
        Ok(roots.collect::<Result<_, _>>()?)
    }

    /// Paths of the given tracks; unknown ids are skipped.
    pub(crate) fn track_paths(&self, ids: &[i64]) -> Result<Vec<(i64, PathBuf)>> {
        let mut stmt = self.conn.prepare("SELECT path FROM track WHERE id = ?1")?;
        let mut paths = Vec::with_capacity(ids.len());
        for &id in ids {
            if let Some(path) = stmt.query_row([id], |r| r.get::<_, String>(0)).optional()? {
                paths.push((id, PathBuf::from(path)));
            }
        }
        Ok(paths)
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM setting WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO setting (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            [key, value],
        )?;
        Ok(())
    }

    pub fn tracks(&self) -> Result<Vec<Track>> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {TRACK_COLUMNS} FROM track ORDER BY id"))?;
        let tracks = stmt.query_map([], track_from_row)?;
        Ok(tracks.collect::<Result<_, _>>()?)
    }
}

pub(crate) fn track_from_row(r: &Row) -> rusqlite::Result<Track> {
    Ok(Track {
        id: r.get(0)?,
        path: PathBuf::from(r.get::<_, String>(1)?),
        state: TrackState::parse(&r.get::<_, String>(2)?),
        error: r.get(3)?,
        meta: Metadata {
            duration_ms: r.get(4)?,
            bitrate: r.get(5)?,
            sample_rate: r.get(6)?,
            channels: r.get(7)?,
            title: r.get(8)?,
            artist: r.get(9)?,
            album_artist: r.get(10)?,
            album: r.get(11)?,
            track_no: r.get(12)?,
            disc_no: r.get(13)?,
            year: r.get(14)?,
            genre: r.get(15)?,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_run_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        drop(Library::open(&path).unwrap());
        let lib = Library::open(&path).unwrap();
        let version: u32 = lib
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version as usize, MIGRATIONS.len());
        assert!(lib.tracks().unwrap().is_empty());
    }
}
