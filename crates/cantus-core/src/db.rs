use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Row};

use crate::{Error, Metadata, Result};

const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/001_init.sql"),
    include_str!("../migrations/002_journal.sql"),
    include_str!("../migrations/003_moves_and_settings.sql"),
    include_str!("../migrations/004_identify.sql"),
    include_str!("../migrations/005_playlists.sql"),
    include_str!("../migrations/006_inbox.sql"),
];

pub(crate) const TRACK_COLUMNS: &str = "id, path, state, error, duration_ms, bitrate, sample_rate, \
     channels, title, artist, album_artist, album, track_no, disc_no, year, genre, recording_mbid, \
     release_mbid";

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
        Self::init(conn, Some(path))
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?, None)
    }

    fn init(mut conn: Connection, path: Option<&Path>) -> Result<Self> {
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.busy_timeout(Duration::from_secs(5))?;

        let version: u32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if let Some(path) = path.filter(|_| version > 0 && (version as usize) < MIGRATIONS.len()) {
            let backup = path.with_extension(format!("v{version}.bak"));
            if !backup.exists() {
                let backup = backup
                    .to_str()
                    .ok_or_else(|| Error::NonUtf8Path(backup.clone()))?;
                conn.execute("VACUUM INTO ?1", [backup])?;
            }
        }
        let tx = conn.transaction()?;
        for (sql, v) in MIGRATIONS.iter().zip(1u32..).skip(version as usize) {
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", v)?;
        }
        tx.commit()?;
        Ok(Self { conn })
    }

    /// Writes a consistent copy of the database to `dest`, replacing any earlier copy.
    pub fn snapshot(&self, dest: &Path) -> Result<()> {
        if let Some(dir) = dest.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = dest.with_extension("tmp");
        let _ = fs::remove_file(&tmp);
        let tmp_str = tmp
            .to_str()
            .ok_or_else(|| Error::NonUtf8Path(tmp.clone()))?;
        self.conn.execute("VACUUM INTO ?1", [tmp_str])?;
        fs::rename(&tmp, dest)?;
        Ok(())
    }

    /// Takes today's backup if there isn't one yet, and keeps only the newest `keep`.
    pub fn daily_backup(&self, dir: &Path, keep: usize) -> Result<()> {
        let today: i64 = self
            .conn
            .query_row("SELECT unixepoch() / 86400", [], |r| r.get(0))?;
        let file = dir.join(format!("library-{today:08}.db"));
        if !file.exists() {
            self.snapshot(&file)?;
        }
        let mut backups: Vec<PathBuf> = fs::read_dir(dir)?
            .filter_map(|entry| Some(entry.ok()?.path()))
            .filter(|path| path.extension().is_some_and(|e| e == "db"))
            .collect();
        backups.sort();
        backups.reverse();
        for old in backups.into_iter().skip(keep) {
            fs::remove_file(old)?;
        }
        Ok(())
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
            recording_mbid: r.get(16)?,
            release_mbid: r.get(17)?,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_can_be_opened_and_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(&dir.path().join("library.db")).unwrap();
        lib.set_setting("key", "first").unwrap();
        let copy = dir.path().join("backups/copy.db");

        lib.snapshot(&copy).unwrap();
        lib.set_setting("key", "second").unwrap();
        lib.snapshot(&copy).unwrap();

        let restored = Library::open(&copy).unwrap();
        assert_eq!(restored.setting("key").unwrap().as_deref(), Some("second"));
    }

    #[test]
    fn an_old_library_is_backed_up_then_upgraded_with_its_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        let old = Connection::open(&path).unwrap();
        old.execute_batch(MIGRATIONS[0]).unwrap();
        old.pragma_update(None, "user_version", 1).unwrap();
        old.execute_batch(
            "INSERT INTO root (path) VALUES ('/m');
             INSERT INTO track (root_id, path, size, mtime, quick_key, state, title)
             VALUES (1, '/m/a.flac', 1, 1, x'00', 'ok', 'Kept');",
        )
        .unwrap();
        drop(old);

        let lib = Library::open(&path).unwrap();

        assert_eq!(lib.tracks().unwrap()[0].meta.title.as_deref(), Some("Kept"));
        let backup = Connection::open(path.with_extension("v1.bak")).unwrap();
        let version: u32 = backup
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, 1);
    }

    #[test]
    fn daily_backups_happen_once_and_are_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(&dir.path().join("library.db")).unwrap();
        let backups = dir.path().join("backups");
        fs::create_dir(&backups).unwrap();
        for day in 1..=9 {
            fs::write(backups.join(format!("library-{day:08}.db")), b"old").unwrap();
        }

        lib.daily_backup(&backups, 7).unwrap();
        lib.daily_backup(&backups, 7).unwrap();

        let mut names: Vec<_> = fs::read_dir(&backups)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names.len(), 7);
        assert_eq!(names[0], "library-00000004.db", "oldest went first");
    }

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
