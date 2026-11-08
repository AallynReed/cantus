use std::collections::HashMap;
use std::fs::{self, File, Metadata as FsMetadata};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use rusqlite::{Connection, Transaction, params};
use walkdir::WalkDir;

use crate::{Error, Library, Result, TrackState, tags};

const AUDIO_EXTENSIONS: &[&str] = &[
    "aac", "aif", "aiff", "ape", "flac", "m4a", "mp3", "oga", "ogg", "opus", "wav", "wv",
];
const QUICK_KEY_CHUNK: u64 = 64 * 1024;

/// What a scan changed. `unreadable` counts files among `added` and `updated` whose tags failed to parse.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ScanReport {
    pub added: usize,
    pub updated: usize,
    pub moved: usize,
    pub missing: usize,
    pub unchanged: usize,
    pub unreadable: usize,
    /// Files or folders that could not be read, or whose path is not UTF-8.
    pub skipped: Vec<PathBuf>,
}

impl ScanReport {
    pub fn merge(&mut self, other: Self) {
        self.added += other.added;
        self.updated += other.updated;
        self.moved += other.moved;
        self.missing += other.missing;
        self.unchanged += other.unchanged;
        self.unreadable += other.unreadable;
        self.skipped.extend(other.skipped);
    }
}

struct Known {
    id: i64,
    size: i64,
    mtime: i64,
    quick_key: Vec<u8>,
    state: TrackState,
}

impl Library {
    /// Adds `root` to the library if new and brings its tracks in line with the disk.
    /// Unchanged files are not read or written. Moved files keep their track id.
    pub fn scan(&mut self, root: &Path) -> Result<ScanReport> {
        let root = std::path::absolute(root)?;
        let root_str = root
            .to_str()
            .ok_or_else(|| Error::NonUtf8Path(root.clone()))?;
        let tx = self.conn.transaction()?;
        tx.execute("INSERT OR IGNORE INTO root (path) VALUES (?1)", [root_str])?;
        let root_id: i64 =
            tx.query_row("SELECT id FROM root WHERE path = ?1", [root_str], |r| {
                r.get(0)
            })?;

        let mut known = known_tracks(&tx, root_id)?;
        let mut report = ScanReport::default();
        let mut new_files = Vec::new();

        for entry in WalkDir::new(&root) {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    report.skipped.extend(err.path().map(Path::to_path_buf));
                    continue;
                }
            };
            if !entry.file_type().is_file() || !is_audio(entry.path()) {
                continue;
            }
            let (Some(path), Ok(fs_meta)) = (entry.path().to_str(), entry.metadata()) else {
                report.skipped.push(entry.into_path());
                continue;
            };
            let (size, mtime) = size_and_mtime(&fs_meta);

            match known.remove(path) {
                Some(k) if k.size == size && k.mtime == mtime && k.state != TrackState::Missing => {
                    report.unchanged += 1;
                }
                Some(k) => {
                    let Ok(key) = quick_key(entry.path(), size) else {
                        report.skipped.push(entry.into_path());
                        continue;
                    };
                    let state = write_track(&tx, Some(k.id), root_id, path, size, mtime, &key)?;
                    report.updated += 1;
                    report.unreadable += usize::from(state == TrackState::Unreadable);
                }
                None => new_files.push((path.to_owned(), size, mtime)),
            }
        }

        let mut vanished: HashMap<Vec<u8>, Vec<Known>> = HashMap::new();
        for k in known.into_values() {
            vanished.entry(k.quick_key.clone()).or_default().push(k);
        }

        for (path, size, mtime) in new_files {
            let Ok(key) = quick_key(Path::new(&path), size) else {
                report.skipped.push(path.into());
                continue;
            };
            if let Some(k) = vanished.get_mut(&key).and_then(Vec::pop) {
                tx.execute(
                    "UPDATE track SET path = ?1, mtime = ?2, state = 'ok' WHERE id = ?3",
                    params![path, mtime, k.id],
                )?;
                report.moved += 1;
            } else {
                let state = write_track(&tx, None, root_id, &path, size, mtime, &key)?;
                report.added += 1;
                report.unreadable += usize::from(state == TrackState::Unreadable);
            }
        }

        for k in vanished.into_values().flatten() {
            if k.state != TrackState::Missing {
                tx.execute("UPDATE track SET state = 'missing' WHERE id = ?1", [k.id])?;
                report.missing += 1;
            }
        }

        tx.commit()?;
        Ok(report)
    }
}

fn known_tracks(tx: &Transaction, root_id: i64) -> Result<HashMap<String, Known>> {
    let mut stmt =
        tx.prepare("SELECT path, id, size, mtime, quick_key, state FROM track WHERE root_id = ?1")?;
    let rows = stmt.query_map([root_id], |r| {
        let state: String = r.get(5)?;
        Ok((
            r.get::<_, String>(0)?,
            Known {
                id: r.get(1)?,
                size: r.get(2)?,
                mtime: r.get(3)?,
                quick_key: r.get(4)?,
                state: if state == "missing" {
                    TrackState::Missing
                } else {
                    TrackState::Ok
                },
            },
        ))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

const TRACK_FIELDS: &str = "root_id, path, size, mtime, quick_key, state, error, duration_ms,      bitrate, sample_rate, channels, title, artist, album_artist, album, track_no, disc_no, year, genre, recording_mbid, release_mbid";
const TRACK_VALUES: &str = "?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21";

/// Reads tags and inserts (`id` = None) or updates a track row. Returns the stored state.
fn write_track(
    conn: &Connection,
    id: Option<i64>,
    root_id: i64,
    path: &str,
    size: i64,
    mtime: i64,
    quick_key: &[u8],
) -> Result<TrackState> {
    let (meta, state, error) = match tags::read(Path::new(path)) {
        Ok(meta) => (meta, TrackState::Ok, None),
        Err(err) => (
            Default::default(),
            TrackState::Unreadable,
            Some(err.to_string()),
        ),
    };
    let state_str = state.as_str();
    let mut values = params![
        root_id,
        path,
        size,
        mtime,
        quick_key,
        state_str,
        error,
        meta.duration_ms,
        meta.bitrate,
        meta.sample_rate,
        meta.channels,
        meta.title,
        meta.artist,
        meta.album_artist,
        meta.album,
        meta.track_no,
        meta.disc_no,
        meta.year,
        meta.genre,
        meta.recording_mbid,
        meta.release_mbid,
    ]
    .to_vec();
    match id {
        None => conn.execute(
            &format!("INSERT INTO track ({TRACK_FIELDS}) VALUES ({TRACK_VALUES})"),
            values.as_slice(),
        )?,
        Some(id) => {
            values.push(&id);
            conn.execute(
                &format!("UPDATE track SET ({TRACK_FIELDS}) = ({TRACK_VALUES}) WHERE id = ?22"),
                values.as_slice(),
            )?
        }
    };
    Ok(state)
}

impl Library {
    /// Adds a file Cantus just placed under a library root. `None` if it vanished.
    pub(crate) fn insert_track(&self, root: &Path, path: &Path) -> Result<Option<i64>> {
        let root_str = root
            .to_str()
            .ok_or_else(|| Error::NonUtf8Path(root.into()))?;
        let path_str = path
            .to_str()
            .ok_or_else(|| Error::NonUtf8Path(path.into()))?;
        let (Ok(fs_meta), root_id) = (
            fs::metadata(path),
            self.conn
                .execute("INSERT OR IGNORE INTO root (path) VALUES (?1)", [root_str]),
        ) else {
            return Ok(None);
        };
        root_id?;
        let root_id: i64 =
            self.conn
                .query_row("SELECT id FROM root WHERE path = ?1", [root_str], |r| {
                    r.get(0)
                })?;
        let (size, mtime) = size_and_mtime(&fs_meta);
        let key = quick_key(path, size)?;
        write_track(&self.conn, None, root_id, path_str, size, mtime, &key)?;
        Ok(Some(self.conn.last_insert_rowid()))
    }

    /// Re-reads a track after Cantus rewrote its file. A file that vanished meanwhile is left
    /// for the next scan to mark missing.
    pub(crate) fn refresh_track(&self, id: i64, path: &Path) -> Result<()> {
        let (Some(path_str), Ok(fs_meta)) = (path.to_str(), fs::metadata(path)) else {
            return Ok(());
        };
        let (size, mtime) = size_and_mtime(&fs_meta);
        let Ok(key) = quick_key(path, size) else {
            return Ok(());
        };
        let root_id: i64 =
            self.conn
                .query_row("SELECT root_id FROM track WHERE id = ?1", [id], |r| {
                    r.get(0)
                })?;
        write_track(&self.conn, Some(id), root_id, path_str, size, mtime, &key)?;
        Ok(())
    }
}

pub(crate) fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO_EXTENSIONS.iter().any(|a| a.eq_ignore_ascii_case(e)))
}

fn size_and_mtime(meta: &FsMetadata) -> (i64, i64) {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
    (i64::try_from(meta.len()).unwrap_or(i64::MAX), mtime)
}

/// Cheap content identity for move detection: size plus the first and last 64 KiB.
fn quick_key(path: &Path, size: i64) -> Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(&size.to_le_bytes());
    hash_chunk(&mut file, &mut hasher)?;
    let len = file.metadata()?.len();
    if len > 2 * QUICK_KEY_CHUNK {
        file.seek(SeekFrom::Start(len - QUICK_KEY_CHUNK))?;
        hash_chunk(&mut file, &mut hasher)?;
    }
    Ok(hasher.finalize().as_bytes().to_vec())
}

fn hash_chunk(file: &mut File, hasher: &mut blake3::Hasher) -> std::io::Result<()> {
    std::io::copy(&mut file.take(QUICK_KEY_CHUNK), hasher).map(drop)
}
