//! Crash-safe, undoable file rewrites.
//!
//! Every rewrite is journaled before the file is touched: `planned` -> `applying` -> `done`.
//! The new bytes go to a synced temp file that is renamed over the original, so the file on
//! disk is always either the old or the new version. `recover` settles rows a crash left
//! mid-step by checking which version is on disk.

use std::ffi::OsString;
use std::fs::{self, File, Permissions};
use std::io::{self, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::params;

use crate::{Error, Library, Result};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct BatchReport {
    pub done: usize,
    /// Files left untouched, with the reason.
    pub failed: Vec<(PathBuf, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeSetSummary {
    pub id: i64,
    /// Unix seconds.
    pub created_at: i64,
    pub description: String,
    pub done: usize,
    pub undone: usize,
    pub failed: usize,
}

/// Outer error aborts the batch (database); inner error fails one file.
type FileOutcome = Result<Result<(), String>>;

impl Library {
    /// Rewrites each `(track_id, path)` through `transform` as one undoable change set.
    pub(crate) fn rewrite_files(
        &mut self,
        description: &str,
        files: Vec<(i64, PathBuf)>,
        mut transform: impl FnMut(&[u8]) -> Result<Vec<u8>, String>,
    ) -> Result<(i64, BatchReport)> {
        self.conn.execute(
            "INSERT INTO change_set (created_at, description) VALUES (?1, ?2)",
            params![unix_now(), description],
        )?;
        let change_set = self.conn.last_insert_rowid();
        let mut report = BatchReport::default();
        for (track_id, path) in files {
            match self.rewrite_one(change_set, track_id, &path, &mut transform)? {
                Ok(()) => report.done += 1,
                Err(reason) => report.failed.push((path, reason)),
            }
        }
        Ok((change_set, report))
    }

    fn rewrite_one(
        &self,
        change_set: i64,
        track_id: i64,
        path: &Path,
        transform: &mut impl FnMut(&[u8]) -> Result<Vec<u8>, String>,
    ) -> FileOutcome {
        let path_str = path
            .to_str()
            .ok_or_else(|| Error::NonUtf8Path(path.into()))?;
        let old = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) => return Ok(Err(err.to_string())),
        };
        self.conn.execute(
            "INSERT INTO journal (change_set_id, track_id, path, step, hash_before)
             VALUES (?1, ?2, ?3, 'planned', ?4)",
            params![change_set, track_id, path_str, hash(&old)],
        )?;
        let id = self.conn.last_insert_rowid();

        // A panic in a format library must fail this file, not the whole batch.
        let new = match panic::catch_unwind(AssertUnwindSafe(|| transform(&old))) {
            Ok(Ok(new)) => new,
            Ok(Err(reason)) => return self.fail(id, reason),
            Err(_) => return self.fail(id, "internal error while writing tags".into()),
        };
        let delta = Delta::between(&old, &new);
        self.conn.execute(
            "UPDATE journal SET step = 'applying', hash_after = ?1, prefix_len = ?2,
                 suffix_len = ?3, old_middle = ?4
             WHERE id = ?5",
            params![
                hash(&new),
                delta.prefix as i64,
                delta.suffix as i64,
                delta.old_middle,
                id
            ],
        )?;

        if let Err(err) = replace_file(path, &new) {
            return self.fail(id, err.to_string());
        }
        self.set_step(id, "done", None)?;
        self.refresh_track(track_id, path)?;
        Ok(Ok(()))
    }

    /// Restores every file of a change set, newest first. Files changed since are skipped.
    pub fn undo(&mut self, change_set: i64) -> Result<BatchReport> {
        let rows = {
            let mut stmt = self.conn.prepare(
                "SELECT id, track_id, path, hash_before, hash_after, prefix_len, suffix_len,
                     old_middle
                 FROM journal WHERE change_set_id = ?1 AND step = 'done' ORDER BY id DESC",
            )?;
            stmt.query_map([change_set], |r| {
                Ok(DoneRow {
                    id: r.get(0)?,
                    track_id: r.get(1)?,
                    path: PathBuf::from(r.get::<_, String>(2)?),
                    hash_before: r.get(3)?,
                    hash_after: r.get(4)?,
                    delta: Delta {
                        prefix: r.get::<_, i64>(5)? as usize,
                        suffix: r.get::<_, i64>(6)? as usize,
                        old_middle: r.get(7)?,
                    },
                })
            })?
            .collect::<Result<Vec<_>, _>>()?
        };

        let mut report = BatchReport::default();
        for row in rows {
            match self.undo_one(&row)? {
                Ok(()) => report.done += 1,
                Err(reason) => report.failed.push((row.path, reason)),
            }
        }
        Ok(report)
    }

    fn undo_one(&self, row: &DoneRow) -> FileOutcome {
        let current = match fs::read(&row.path) {
            Ok(bytes) => bytes,
            Err(err) => return Ok(Err(err.to_string())),
        };
        if hash(&current) != row.hash_after {
            return Ok(Err("file changed since the edit, left as is".into()));
        }
        let Some(old) = row
            .delta
            .rebuild(&current)
            .filter(|old| hash(old) == row.hash_before)
        else {
            return Ok(Err("journal does not match the file, left as is".into()));
        };

        self.set_step(row.id, "undoing", None)?;
        if let Err(err) = replace_file(&row.path, &old) {
            self.set_step(row.id, "done", Some(&err.to_string()))?;
            return Ok(Err(err.to_string()));
        }
        self.set_step(row.id, "undone", None)?;
        if let Some(track_id) = row.track_id {
            self.refresh_track(track_id, &row.path)?;
        }
        Ok(Ok(()))
    }

    /// Settles journal rows a crash left mid-step. Safe to run any number of times.
    /// Returns how many rows it settled.
    pub fn recover(&mut self) -> Result<usize> {
        let rows = {
            let mut stmt = self.conn.prepare(
                "SELECT id, track_id, path, step, hash_before, hash_after FROM journal
                 WHERE step IN ('planned', 'applying', 'undoing')",
            )?;
            stmt.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    PathBuf::from(r.get::<_, String>(2)?),
                    r.get::<_, String>(3)?,
                    r.get::<_, Vec<u8>>(4)?,
                    r.get::<_, Option<Vec<u8>>>(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };

        for (id, track_id, path, step, before, after) in &rows {
            // A leftover temp file is never the only copy: the original is replaced by rename.
            let _ = fs::remove_file(temp_path(path));
            let on_disk = fs::read(path).ok().map(|bytes| hash(&bytes));
            let is = |h: &[u8]| on_disk.as_deref() == Some(h);
            let (new_step, error) = match step.as_str() {
                "planned" => ("failed", Some("interrupted before writing")),
                _ if after.as_deref().is_some_and(is) => ("done", None),
                "applying" if is(before) => ("failed", Some("interrupted, file left unchanged")),
                "applying" => ("failed", Some("interrupted, file changed or missing")),
                _ if is(before) => ("undone", None),
                _ => ("done", Some("undo interrupted, file changed or missing")),
            };
            self.set_step(*id, new_step, error)?;
            if let Some(track_id) = track_id {
                self.refresh_track(*track_id, path)?;
            }
        }
        Ok(rows.len())
    }

    /// Change sets, newest first.
    pub fn history(&self) -> Result<Vec<ChangeSetSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.id, c.created_at, c.description,
                 COUNT(*) FILTER (WHERE j.step = 'done'),
                 COUNT(*) FILTER (WHERE j.step = 'undone'),
                 COUNT(*) FILTER (WHERE j.step = 'failed')
             FROM change_set c LEFT JOIN journal j ON j.change_set_id = c.id
             GROUP BY c.id ORDER BY c.id DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(ChangeSetSummary {
                id: r.get(0)?,
                created_at: r.get(1)?,
                description: r.get(2)?,
                done: r.get::<_, i64>(3)? as usize,
                undone: r.get::<_, i64>(4)? as usize,
                failed: r.get::<_, i64>(5)? as usize,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    fn fail(&self, id: i64, reason: String) -> FileOutcome {
        self.set_step(id, "failed", Some(&reason))?;
        Ok(Err(reason))
    }

    fn set_step(&self, id: i64, step: &str, error: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE journal SET step = ?1, error = ?2 WHERE id = ?3",
            params![step, error, id],
        )?;
        Ok(())
    }
}

struct DoneRow {
    id: i64,
    track_id: Option<i64>,
    path: PathBuf,
    hash_before: Vec<u8>,
    hash_after: Vec<u8>,
    delta: Delta,
}

/// The bytes of the old file that differ from the new one, enough to rebuild the old file.
/// Tag edits usually touch only the start or end, so this stays small.
#[derive(Debug)]
struct Delta {
    prefix: usize,
    suffix: usize,
    old_middle: Vec<u8>,
}

impl Delta {
    fn between(old: &[u8], new: &[u8]) -> Self {
        let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
        let suffix = old
            .iter()
            .rev()
            .zip(new.iter().rev())
            .take(old.len().min(new.len()) - prefix)
            .take_while(|(a, b)| a == b)
            .count();
        Self {
            prefix,
            suffix,
            old_middle: old[prefix..old.len() - suffix].to_vec(),
        }
    }

    fn rebuild(&self, new: &[u8]) -> Option<Vec<u8>> {
        let tail = new.len().checked_sub(self.suffix)?;
        let head = new.get(..self.prefix)?;
        (self.prefix <= tail).then(|| [head, &self.old_middle, &new[tail..]].concat())
    }
}

fn hash(bytes: &[u8]) -> Vec<u8> {
    blake3::hash(bytes).as_bytes().to_vec()
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().try_into().unwrap_or(i64::MAX))
}

fn temp_path(path: &Path) -> PathBuf {
    let mut name = OsString::from(".");
    name.push(path.file_name().unwrap_or_default());
    name.push(".cantus-tmp");
    path.with_file_name(name)
}

/// Writes `bytes` to a synced temp file beside `path`, then renames it over `path`.
fn replace_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = temp_path(path);
    let result = fs::metadata(path)
        .and_then(|meta| write_synced(&tmp, bytes, meta.permissions()))
        .and_then(|()| fs::rename(&tmp, path))
        .and_then(|()| sync_parent(path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn write_synced(path: &Path, bytes: &[u8], permissions: Permissions) -> io::Result<()> {
    let mut file = File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    file.set_permissions(permissions)
}

/// Makes the rename itself durable. Windows has no directory handles to sync.
#[cfg(unix)]
fn sync_parent(path: &Path) -> io::Result<()> {
    File::open(path.parent().unwrap_or(Path::new(".")))?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent(_: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn delta_rebuilds_the_old_bytes(old: Vec<u8>, new: Vec<u8>) {
            let delta = Delta::between(&old, &new);
            prop_assert_eq!(delta.rebuild(&new), Some(old));
        }

        #[test]
        fn delta_of_a_small_edit_is_small(
            mut bytes in prop::collection::vec(any::<u8>(), 100..2000),
            at in 0usize..100,
            patch: [u8; 4],
        ) {
            let old = bytes.clone();
            bytes.splice(at..at, patch);
            let delta = Delta::between(&old, &bytes);
            prop_assert!(delta.old_middle.is_empty());
            prop_assert_eq!(delta.rebuild(&bytes), Some(old));
        }
    }

    fn library_with_track(path: &Path) -> Library {
        let lib = Library::open_in_memory().unwrap();
        lib.conn
            .execute("INSERT INTO root (path) VALUES ('/')", [])
            .unwrap();
        lib.conn
            .execute(
                "INSERT INTO track (root_id, path, size, mtime, quick_key, state)
                 VALUES (1, ?1, 0, 0, x'', 'ok')",
                [path.to_str().unwrap()],
            )
            .unwrap();
        lib
    }

    /// A library with one edited file, plus the file's original and edited bytes.
    fn edited() -> (tempfile::TempDir, Library, PathBuf, Vec<u8>, Vec<u8>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.bin");
        fs::write(&path, b"header-old-audio").unwrap();
        let mut lib = library_with_track(&path);
        let (_, report) = lib
            .rewrite_files("edit", vec![(1, path.clone())], |b| {
                Ok(String::from_utf8_lossy(b)
                    .replace("old", "new!")
                    .into_bytes())
            })
            .unwrap();
        assert_eq!(report.done, 1);
        (
            dir,
            lib,
            path,
            b"header-old-audio".to_vec(),
            b"header-new!-audio".to_vec(),
        )
    }

    fn step(lib: &Library) -> (String, Option<String>) {
        lib.conn
            .query_row("SELECT step, error FROM journal", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap()
    }

    fn force_step(lib: &Library, step: &str) {
        lib.conn
            .execute("UPDATE journal SET step = ?1", [step])
            .unwrap();
    }

    #[test]
    fn undo_restores_bytes_and_skips_files_changed_since() {
        let (_dir, mut lib, path, old, _) = edited();
        assert_eq!(lib.undo(1).unwrap().done, 1);
        assert_eq!(fs::read(&path).unwrap(), old);
        assert_eq!(step(&lib).0, "undone");

        let (_dir, mut lib, path, _, _) = edited();
        fs::write(&path, b"changed by someone else").unwrap();
        let report = lib.undo(1).unwrap();
        assert_eq!((report.done, report.failed.len()), (0, 1));
        assert_eq!(fs::read(&path).unwrap(), b"changed by someone else");
        assert_eq!(step(&lib).0, "done");
    }

    #[test]
    fn failed_transform_leaves_the_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.bin");
        fs::write(&path, b"data").unwrap();
        let mut lib = library_with_track(&path);
        let (_, report) = lib
            .rewrite_files("edit", vec![(1, path.clone())], |_| Err("bad".into()))
            .unwrap();
        assert_eq!(report.failed, vec![(path.clone(), "bad".to_owned())]);
        assert_eq!(fs::read(&path).unwrap(), b"data");
        assert_eq!(step(&lib), ("failed".into(), Some("bad".into())));
    }

    #[test]
    fn a_panicking_transform_fails_only_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a.bin"), dir.path().join("b.bin"));
        fs::write(&a, b"a").unwrap();
        fs::write(&b, b"b").unwrap();
        let mut lib = library_with_track(&a);
        let (_, report) = lib
            .rewrite_files("edit", vec![(1, a.clone()), (1, b.clone())], |bytes| {
                assert_ne!(bytes, b"a", "simulated library bug");
                Ok(b"B".to_vec())
            })
            .unwrap();
        assert_eq!(report.done, 1);
        assert_eq!(report.failed[0].0, a);
        assert_eq!(fs::read(&a).unwrap(), b"a");
        assert_eq!(fs::read(&b).unwrap(), b"B");
    }

    #[test]
    fn recovery_settles_every_interrupted_state() {
        // (state on disk after the crash, interrupted step, expected step)
        let cases: [(&str, &str, &str); 7] = [
            ("old", "planned", "failed"),
            ("old", "applying", "failed"),
            ("new", "applying", "done"),
            ("other", "applying", "failed"),
            ("old", "undoing", "undone"),
            ("new", "undoing", "done"),
            ("other", "undoing", "done"),
        ];
        for (disk, interrupted, expected) in cases {
            let (_dir, mut lib, path, old, new) = edited();
            let bytes = match disk {
                "old" => old,
                "new" => new.clone(),
                _ => b"other".to_vec(),
            };
            fs::write(&path, &bytes).unwrap();
            fs::write(temp_path(&path), &new).unwrap();
            force_step(&lib, interrupted);

            assert_eq!(lib.recover().unwrap(), 1, "{disk} {interrupted}");
            assert_eq!(step(&lib).0, expected, "{disk} {interrupted}");
            assert_eq!(
                fs::read(&path).unwrap(),
                bytes,
                "recovery never rewrites files"
            );
            assert!(!temp_path(&path).exists());
            assert_eq!(lib.recover().unwrap(), 0, "second run is a no-op");
        }
    }

    #[test]
    fn history_counts_steps() {
        let (_dir, mut lib, _, _, _) = edited();
        lib.undo(1).unwrap();
        let history = lib.history().unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(
            (history[0].done, history[0].undone, history[0].failed),
            (0, 1, 0)
        );
        assert_eq!(history[0].description, "edit");
    }
}
