//! Crash-safe, undoable file rewrites and moves.
//!
//! Every change is journaled before the file is touched: `planned` -> `applying` -> `done`.
//! New bytes go to a synced temp file that is renamed over the original, so the file on disk
//! is always either the old or the new version. Moves never overwrite, and a move across
//! volumes deletes the source only after verifying the copy. `recover` settles rows a crash
//! left mid-step by checking what is on disk.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{OptionalExtension, params};

use crate::{Error, Handoff, Library, PlannedMove, Result};

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
        mut transform: impl FnMut(i64, &[u8]) -> Result<Vec<u8>, String>,
    ) -> Result<(i64, BatchReport)> {
        let change_set = self.new_change_set(description)?;
        let mut report = BatchReport::default();
        for (track_id, path) in files {
            match self.rewrite_one(change_set, track_id, &path, &mut transform)? {
                Ok(()) => report.done += 1,
                Err(reason) => report.failed.push((path, reason)),
            }
        }
        Ok((change_set, report))
    }

    /// Moves files as one undoable change set, then removes source folders left empty.
    pub(crate) fn move_files(
        &mut self,
        description: &str,
        moves: Vec<PlannedMove>,
    ) -> Result<(i64, BatchReport)> {
        let change_set = self.new_change_set(description)?;
        let mut report = BatchReport::default();
        for m in moves {
            match self.move_one(change_set, &m)? {
                Ok(()) => report.done += 1,
                Err(reason) => report.failed.push((m.from, reason)),
            }
        }
        Ok((change_set, report))
    }

    /// Copies new files into the library as one undoable change set. Sources are not touched.
    pub(crate) fn copy_files(
        &mut self,
        description: &str,
        files: Vec<PlannedMove>,
        handoff: Handoff,
    ) -> Result<(i64, BatchReport)> {
        let change_set = self.new_change_set(description)?;
        let mut report = BatchReport::default();
        for m in files {
            match self.copy_one(change_set, &m, handoff)? {
                Ok(()) => report.done += 1,
                Err(reason) => report.failed.push((m.from, reason)),
            }
        }
        Ok((change_set, report))
    }

    fn copy_one(&self, change_set: i64, m: &PlannedMove, handoff: Handoff) -> FileOutcome {
        let digest = match hash_file(&m.from) {
            Ok(digest) => digest,
            Err(err) => return Ok(Err(err.to_string())),
        };
        self.conn.execute(
            "INSERT INTO journal (change_set_id, path, new_path, step, hash_before, op)
             VALUES (?1, ?2, ?3, 'applying', ?4, 'copy')",
            params![change_set, utf8(&m.from)?, utf8(&m.to)?, digest],
        )?;
        let id = self.conn.last_insert_rowid();
        if let Err(err) = place_file(&m.from, &m.to, &digest, handoff) {
            return self.fail(id, err.to_string());
        }
        self.finish_copy(id, &m.root, &m.to)?;
        Ok(Ok(()))
    }

    fn finish_copy(&self, id: i64, root: &Path, to: &Path) -> Result<()> {
        let track_id = self.insert_track(root, to)?;
        self.conn.execute(
            "UPDATE journal SET track_id = ?1, step = 'done', error = NULL WHERE id = ?2",
            params![track_id, id],
        )?;
        Ok(())
    }

    fn undo_copy(&self, row: &DoneRow, copy: &Path) -> FileOutcome {
        if hash_file(copy).ok().as_ref() != Some(&row.hash_before) {
            return Ok(Err("file changed since the import, left as is".into()));
        }
        let root = row.track_id.map_or(Ok(None), |id| self.track_root(id))?;
        self.set_step(row.id, "undoing", None)?;
        if let Err(err) = fs::remove_file(copy) {
            self.set_step(row.id, "done", Some(&err.to_string()))?;
            return Ok(Err(err.to_string()));
        }
        self.settle_undone_copy(row.id, row.track_id)?;
        if let Some(root) = root {
            prune_empty_dirs(copy, &root);
        }
        Ok(Ok(()))
    }

    fn settle_undone_copy(&self, id: i64, track_id: Option<i64>) -> Result<()> {
        if let Some(track_id) = track_id {
            self.conn
                .execute("DELETE FROM track WHERE id = ?1", [track_id])?;
        }
        self.set_step(id, "undone", None)
    }

    fn settle_copy(
        &self,
        id: i64,
        track_id: Option<i64>,
        to: &Path,
        step: &str,
        digest: &[u8],
    ) -> Result<()> {
        let _ = fs::remove_file(temp_path(to));
        let intact = hash_file(to).is_ok_and(|h| h == digest);
        match (step, intact) {
            ("undoing", false) if !to.exists() => self.settle_undone_copy(id, track_id),
            ("undoing", _) => self.set_step(id, "done", Some("undo interrupted, file kept")),
            (_, true) if track_id.is_some() => self.set_step(id, "done", None),
            (_, true) => {
                let root = self
                    .roots()?
                    .into_iter()
                    .find(|root| to.starts_with(root))
                    .unwrap_or_else(|| to.parent().unwrap_or(to).to_path_buf());
                self.finish_copy(id, &root, to)
            }
            _ => self.set_step(id, "failed", Some("interrupted, file not imported")),
        }
    }

    fn new_change_set(&self, description: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO change_set (created_at, description) VALUES (?1, ?2)",
            params![unix_now(), description],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    fn move_one(&self, change_set: i64, m: &PlannedMove) -> FileOutcome {
        let from = utf8(&m.from)?;
        let to = utf8(&m.to)?;
        self.conn.execute(
            "INSERT INTO journal (change_set_id, track_id, path, new_path, step, hash_before)
             VALUES (?1, ?2, ?3, ?4, 'applying', x'')",
            params![change_set, m.track_id, from, to],
        )?;
        let id = self.conn.last_insert_rowid();
        if let Err(reason) = self.perform_move(id, &m.from, &m.to)? {
            return self.fail(id, reason);
        }
        self.set_step(id, "done", None)?;
        self.set_track_path(m.track_id, to)?;
        prune_empty_dirs(&m.from, &m.root);
        Ok(Ok(()))
    }

    /// Renames `from` to `to`, falling back to a verified copy across volumes.
    fn perform_move(&self, id: i64, from: &Path, to: &Path) -> FileOutcome {
        let moved = match rename_no_clobber(from, to) {
            Err(err) if err.kind() == io::ErrorKind::CrossesDevices => match hash_file(from) {
                Ok(digest) => {
                    // Recorded first so recovery can tell a finished copy from a partial one.
                    self.conn.execute(
                        "UPDATE journal SET hash_before = ?1 WHERE id = ?2",
                        params![digest, id],
                    )?;
                    copy_then_remove(from, to, &digest)
                }
                Err(err) => Err(err),
            },
            other => other,
        };
        Ok(moved.map_err(|err| err.to_string()))
    }

    fn rewrite_one(
        &self,
        change_set: i64,
        track_id: i64,
        path: &Path,
        transform: &mut impl FnMut(i64, &[u8]) -> Result<Vec<u8>, String>,
    ) -> FileOutcome {
        let path_str = utf8(path)?;
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
        let new = match panic::catch_unwind(AssertUnwindSafe(|| transform(track_id, &old))) {
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
                     old_middle, new_path, op
                 FROM journal WHERE change_set_id = ?1 AND step = 'done' ORDER BY id DESC",
            )?;
            stmt.query_map([change_set], |r| {
                Ok(DoneRow {
                    id: r.get(0)?,
                    track_id: r.get(1)?,
                    path: PathBuf::from(r.get::<_, String>(2)?),
                    hash_before: r.get(3)?,
                    hash_after: r.get(4)?,
                    delta: match (r.get::<_, Option<i64>>(5)?, r.get::<_, Option<i64>>(6)?) {
                        (Some(prefix), Some(suffix)) => Some(Delta {
                            prefix: prefix as usize,
                            suffix: suffix as usize,
                            old_middle: r.get(7)?,
                        }),
                        _ => None,
                    },
                    new_path: r.get::<_, Option<String>>(8)?.map(PathBuf::from),
                    op: r.get(9)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?
        };

        let mut report = BatchReport::default();
        for row in rows {
            let outcome = match (&row.new_path, row.op.as_deref()) {
                (Some(copy), Some("copy")) => self.undo_copy(&row, copy)?,
                (Some(new_path), _) => self.undo_move(&row, new_path)?,
                (None, _) => self.undo_rewrite(&row)?,
            };
            match outcome {
                Ok(()) => report.done += 1,
                Err(reason) => report.failed.push((row.path, reason)),
            }
        }
        if report.failed.is_empty() {
            self.conn.execute(
                "UPDATE inbox SET change_set_id = NULL WHERE change_set_id = ?1",
                [change_set],
            )?;
        }
        Ok(report)
    }

    fn undo_rewrite(&self, row: &DoneRow) -> FileOutcome {
        // The track may have been organized elsewhere since this edit.
        let path = self.current_path(row)?;
        let current = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) => return Ok(Err(err.to_string())),
        };
        if row.hash_after.as_deref() != Some(hash(&current).as_slice()) {
            return Ok(Err("file changed since the edit, left as is".into()));
        }
        let Some(old) = row
            .delta
            .as_ref()
            .and_then(|delta| delta.rebuild(&current))
            .filter(|old| hash(old) == row.hash_before)
        else {
            return Ok(Err("journal does not match the file, left as is".into()));
        };

        self.set_step(row.id, "undoing", None)?;
        if let Err(err) = replace_file(&path, &old) {
            self.set_step(row.id, "done", Some(&err.to_string()))?;
            return Ok(Err(err.to_string()));
        }
        self.set_step(row.id, "undone", None)?;
        if let Some(track_id) = row.track_id {
            self.refresh_track(track_id, &path)?;
        }
        Ok(Ok(()))
    }

    fn undo_move(&self, row: &DoneRow, new_path: &Path) -> FileOutcome {
        self.set_step(row.id, "undoing", None)?;
        if let Err(reason) = self.perform_move(row.id, new_path, &row.path)? {
            self.set_step(row.id, "done", Some(&reason))?;
            return Ok(Err(reason));
        }
        self.set_step(row.id, "undone", None)?;
        if let Some(track_id) = row.track_id {
            self.set_track_path(track_id, utf8(&row.path)?)?;
            if let Some(root) = self.track_root(track_id)? {
                prune_empty_dirs(new_path, &root);
            }
        }
        Ok(Ok(()))
    }

    fn current_path(&self, row: &DoneRow) -> Result<PathBuf> {
        let tracked = row.track_id.map_or(Ok(None), |id| {
            self.conn
                .query_row("SELECT path FROM track WHERE id = ?1", [id], |r| {
                    r.get::<_, String>(0)
                })
                .optional()
        })?;
        Ok(tracked.map_or_else(|| row.path.clone(), PathBuf::from))
    }

    fn track_root(&self, track_id: i64) -> Result<Option<PathBuf>> {
        let root = self
            .conn
            .query_row(
                "SELECT r.path FROM root r JOIN track t ON t.root_id = r.id WHERE t.id = ?1",
                [track_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(root.map(PathBuf::from))
    }

    fn set_track_path(&self, track_id: i64, path: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE track SET path = ?1 WHERE id = ?2",
            params![path, track_id],
        )?;
        Ok(())
    }

    /// Settles journal rows a crash left mid-step. Safe to run any number of times.
    /// Returns how many rows it settled.
    pub fn recover(&mut self) -> Result<usize> {
        let rows = {
            let mut stmt = self.conn.prepare(
                "SELECT id, track_id, path, step, hash_before, hash_after, new_path, op FROM journal
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
                    r.get::<_, Option<String>>(6)?.map(PathBuf::from),
                    r.get::<_, Option<String>>(7)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };

        for (id, track_id, path, step, before, after, new_path, op) in &rows {
            if let (Some(copy), Some("copy")) = (new_path, op.as_deref()) {
                self.settle_copy(*id, *track_id, copy, step, before)?;
                continue;
            }
            if let Some(new_path) = new_path {
                self.settle_move(*id, *track_id, path, new_path, step, before)?;
                continue;
            }
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

    fn settle_move(
        &self,
        id: i64,
        track_id: Option<i64>,
        old: &Path,
        new: &Path,
        step: &str,
        digest: &[u8],
    ) -> Result<()> {
        let applying = step == "applying";
        let (from, to) = if applying { (old, new) } else { (new, old) };
        let _ = fs::remove_file(temp_path(to));
        let finished = match (from.exists(), to.exists()) {
            (false, true) => true,
            // Both exist only between a verified copy and deleting the source.
            (true, true) => {
                !digest.is_empty()
                    && hash_file(to).is_ok_and(|h| h == digest)
                    && fs::remove_file(from).is_ok()
            }
            _ => false,
        };
        let (new_step, error, path) = match (applying, finished) {
            (true, true) => ("done", None, new),
            (true, false) => ("failed", Some("interrupted, file not moved"), old),
            (false, true) => ("undone", None, old),
            (false, false) => ("done", Some("undo interrupted, file not moved"), new),
        };
        self.set_step(id, new_step, error)?;
        if let Some(track_id) = track_id {
            self.set_track_path(track_id, utf8(path)?)?;
        }
        Ok(())
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
    hash_after: Option<Vec<u8>>,
    delta: Option<Delta>,
    new_path: Option<PathBuf>,
    op: Option<String>,
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

fn hash_file(path: &Path) -> io::Result<Vec<u8>> {
    let mut hasher = blake3::Hasher::new();
    io::copy(&mut File::open(path)?, &mut hasher)?;
    Ok(hasher.finalize().as_bytes().to_vec())
}

fn utf8(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| Error::NonUtf8Path(path.to_path_buf()))
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

/// Rewrites an existing file atomically, keeping its permissions.
fn replace_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    swap_in(path, bytes, Some(fs::metadata(path)?.permissions()))
}

/// Creates or replaces a file atomically.
pub(crate) fn write_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    swap_in(
        path,
        bytes,
        fs::metadata(path).ok().map(|m| m.permissions()),
    )
}

/// Writes `bytes` to a synced temp file beside `path`, then renames it over `path`.
fn swap_in(path: &Path, bytes: &[u8], permissions: Option<Permissions>) -> io::Result<()> {
    let tmp = temp_path(path);
    let result = write_synced(&tmp, bytes, permissions)
        .and_then(|()| fs::rename(&tmp, path))
        .and_then(|()| sync_parent(path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Renames without replacing anything, except `from` itself (a case-only rename).
fn rename_no_clobber(from: &Path, to: &Path) -> io::Result<()> {
    if to.exists() && !same_file::is_same_file(from, to)? {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "destination already exists",
        ));
    }
    if let Some(dir) = to.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::rename(from, to)?;
    sync_parent(to)
}

/// Moves across volumes. The source is untouched unless the copy is complete and correct.
fn copy_then_remove(from: &Path, to: &Path, digest: &[u8]) -> io::Result<()> {
    copy_verified(from, to, digest)?;
    fs::remove_file(from)
}

/// Puts a new file in the library without replacing anything or changing the source.
fn place_file(from: &Path, to: &Path, digest: &[u8], handoff: Handoff) -> io::Result<()> {
    if to.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "destination already exists",
        ));
    }
    if let Some(dir) = to.parent() {
        fs::create_dir_all(dir)?;
    }
    if handoff == Handoff::Hardlink && fs::hard_link(from, to).is_ok() {
        return sync_parent(to);
    }
    copy_verified(from, to, digest)
}

/// Copies to a temp file, verifies it against `digest`, then renames it into place.
fn copy_verified(from: &Path, to: &Path, digest: &[u8]) -> io::Result<()> {
    let tmp = temp_path(to);
    let copied = fs::copy(from, &tmp)
        // Windows can only flush a handle opened for writing.
        .and_then(|_| OpenOptions::new().write(true).open(&tmp)?.sync_all())
        .and_then(|()| hash_file(&tmp))
        .and_then(|actual| {
            if actual == digest {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "copy does not match the source",
                ))
            }
        })
        .and_then(|()| fs::rename(&tmp, to))
        .and_then(|()| sync_parent(to));
    if copied.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    copied
}

/// Removes `file`'s parent folders up to `root`, stopping at the first that is not empty.
fn prune_empty_dirs(file: &Path, root: &Path) {
    for dir in file
        .ancestors()
        .skip(1)
        .take_while(|d| *d != root && d.starts_with(root))
    {
        if fs::remove_dir(dir).is_err() {
            break;
        }
    }
}

fn write_synced(path: &Path, bytes: &[u8], permissions: Option<Permissions>) -> io::Result<()> {
    let mut file = File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    permissions.map_or(Ok(()), |p| file.set_permissions(p))
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
            .rewrite_files("edit", vec![(1, path.clone())], |_, b| {
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
            .rewrite_files("edit", vec![(1, path.clone())], |_, _| Err("bad".into()))
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
            .rewrite_files("edit", vec![(1, a.clone()), (1, b.clone())], |_, bytes| {
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

    /// A library with one file moved from `a.bin` to `sub/b.bin`.
    fn moved() -> (tempfile::TempDir, Library, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("a.bin");
        let to = dir.path().join("sub/b.bin");
        fs::write(&from, b"audio").unwrap();
        let mut lib = library_with_track(&from);
        let m = PlannedMove {
            track_id: 1,
            root: dir.path().to_path_buf(),
            from: from.clone(),
            to: to.clone(),
            collision: false,
        };
        assert_eq!(lib.move_files("move", vec![m]).unwrap().1.done, 1);
        (dir, lib, from, to)
    }

    #[test]
    fn verified_copy_removes_the_source_only_when_it_matches() {
        let dir = tempfile::tempdir().unwrap();
        let (from, to) = (dir.path().join("a"), dir.path().join("b"));
        fs::write(&from, b"audio").unwrap();

        assert!(copy_then_remove(&from, &to, &hash(b"something else")).is_err());
        assert_eq!(fs::read(&from).unwrap(), b"audio");
        assert!(!to.exists() && !temp_path(&to).exists());

        copy_then_remove(&from, &to, &hash(b"audio")).unwrap();
        assert_eq!(fs::read(&to).unwrap(), b"audio");
        assert!(!from.exists());
    }

    #[test]
    fn rename_never_replaces_another_file() {
        let dir = tempfile::tempdir().unwrap();
        let (from, to) = (dir.path().join("a"), dir.path().join("b"));
        fs::write(&from, b"mine").unwrap();
        fs::write(&to, b"theirs").unwrap();
        assert!(rename_no_clobber(&from, &to).is_err());
        assert_eq!(fs::read(&to).unwrap(), b"theirs");
        assert_eq!(fs::read(&from).unwrap(), b"mine");
    }

    #[test]
    fn recovery_settles_every_interrupted_move() {
        // (source exists, destination holds, digest matches, step, expected, track ends at new)
        let good = hash(b"audio");
        type Case<'a> = (bool, Option<&'a [u8]>, &'a [u8], &'a str, &'a str, bool);
        let cases: [Case; 7] = [
            (true, None, b"", "applying", "failed", false),
            (false, Some(b"audio"), b"", "applying", "done", true),
            (true, Some(b"audio"), &good, "applying", "done", true),
            (true, Some(b"junk"), &good, "applying", "failed", false),
            (false, None, b"", "applying", "failed", false),
            (false, Some(b"audio"), b"", "undoing", "done", true),
            (true, None, b"", "undoing", "undone", false),
        ];
        for (old_exists, new_holds, digest, interrupted, expected, at_new) in cases {
            let (_dir, mut lib, old, new) = moved();
            fs::remove_file(&new).unwrap();
            if old_exists {
                fs::write(&old, b"audio").unwrap();
            }
            if let Some(bytes) = new_holds {
                fs::write(&new, bytes).unwrap();
            }
            // Undoing moves new -> old, so a "digest match" only applies to applying here.
            lib.conn
                .execute(
                    "UPDATE journal SET step = ?1, hash_before = ?2",
                    params![interrupted, digest],
                )
                .unwrap();

            assert_eq!(lib.recover().unwrap(), 1);
            let label = format!("{old_exists} {new_holds:?} {interrupted}");
            assert_eq!(step(&lib).0, expected, "{label}");
            let tracked = lib.tracks().unwrap()[0].path.clone();
            assert_eq!(
                tracked,
                if at_new { new.clone() } else { old.clone() },
                "{label}"
            );
            assert_eq!(lib.recover().unwrap(), 0, "{label}");
        }
    }

    #[test]
    fn recovery_settles_every_interrupted_import() {
        // (destination holds, step, expected step, track exists afterwards)
        type Case<'a> = (Option<&'a [u8]>, &'a str, &'a str, bool);
        let cases: [Case; 5] = [
            (None, "applying", "failed", false),
            (Some(b"partial"), "applying", "failed", false),
            (Some(b"audio"), "applying", "done", true),
            (None, "undoing", "undone", false),
            (Some(b"audio"), "undoing", "done", true),
        ];
        for (holds, interrupted, expected, tracked) in cases {
            let dir = tempfile::tempdir().unwrap();
            let lib_root = dir.path().join("lib");
            fs::create_dir(&lib_root).unwrap();
            let (from, to) = (dir.path().join("a.bin"), lib_root.join("b.bin"));
            fs::write(&from, b"audio").unwrap();
            let mut lib = Library::open_in_memory().unwrap();
            let m = PlannedMove {
                track_id: 0,
                root: lib_root.clone(),
                from: from.clone(),
                to: to.clone(),
                collision: false,
            };
            lib.copy_files("import", vec![m], Handoff::Copy).unwrap();
            lib.conn
                .execute("UPDATE journal SET step = ?1", [interrupted])
                .unwrap();
            fs::remove_file(&to).unwrap();
            if interrupted == "applying" {
                lib.conn.execute("DELETE FROM track", []).unwrap();
                lib.conn
                    .execute("UPDATE journal SET track_id = NULL", [])
                    .unwrap();
            }
            if let Some(bytes) = holds {
                fs::write(&to, bytes).unwrap();
            }

            assert_eq!(lib.recover().unwrap(), 1);

            let label = format!("{holds:?} {interrupted}");
            assert_eq!(step(&lib).0, expected, "{label}");
            assert_eq!(!lib.tracks().unwrap().is_empty(), tracked, "{label}");
            assert_eq!(
                fs::read(&from).unwrap(),
                b"audio",
                "{label}: source untouched"
            );
            assert_eq!(lib.recover().unwrap(), 0, "{label}");
        }
    }

    #[test]
    fn undoing_a_move_restores_the_path_and_removes_new_folders() {
        let (dir, mut lib, from, to) = moved();
        lib.conn
            .execute("UPDATE root SET path = ?1", [dir.path().to_str().unwrap()])
            .unwrap();
        assert_eq!(lib.undo(1).unwrap().done, 1);
        assert_eq!(fs::read(&from).unwrap(), b"audio");
        assert!(!to.parent().unwrap().exists());
        assert_eq!(lib.tracks().unwrap()[0].path, from);
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
