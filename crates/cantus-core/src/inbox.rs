//! The Inbox holds finished downloads until they are imported into the library.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use rusqlite::params;

use crate::organize::{free_path, key};
use crate::scan::is_audio;
use crate::tags;
use crate::{
    BatchReport, Error, Library, PlannedMove, Result, Template, Torrents, Track, TrackState,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboxBatch {
    pub id: i64,
    pub origin: String,
    pub name: String,
    pub files: Vec<PathBuf>,
    /// The change set that imported it. Undoing that returns the batch to the Inbox.
    pub imported_by: Option<i64>,
}

/// How a download reaches the library. The source file is never modified either way, so a
/// torrent keeps seeding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handoff {
    Copy,
    /// Saves disk space on the same volume; falls back to copying elsewhere.
    Hardlink,
}

impl Library {
    pub fn add_to_inbox(&mut self, origin: &str, name: &str, files: &[PathBuf]) -> Result<i64> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO inbox (origin, name, created_at) VALUES (?1, ?2, unixepoch())",
            params![origin, name],
        )?;
        let id = tx.last_insert_rowid();
        for file in files {
            let path = file
                .to_str()
                .ok_or_else(|| Error::NonUtf8Path(file.clone()))?;
            tx.execute(
                "INSERT OR IGNORE INTO inbox_file (inbox_id, path) VALUES (?1, ?2)",
                params![id, path],
            )?;
        }
        tx.commit()?;
        Ok(id)
    }

    /// Batches, newest first.
    pub fn inbox(&self) -> Result<Vec<InboxBatch>> {
        let mut batches = self
            .conn
            .prepare("SELECT id, origin, name, change_set_id FROM inbox ORDER BY id DESC")?;
        let mut files = self
            .conn
            .prepare("SELECT path FROM inbox_file WHERE inbox_id = ?1 ORDER BY path")?;
        let rows = batches
            .query_map([], |r| {
                Ok(InboxBatch {
                    id: r.get(0)?,
                    origin: r.get(1)?,
                    name: r.get(2)?,
                    files: Vec::new(),
                    imported_by: r.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|mut batch| {
                batch.files = files
                    .query_map([batch.id], |r| r.get::<_, String>(0).map(PathBuf::from))?
                    .collect::<Result<_, _>>()?;
                Ok(batch)
            })
            .collect()
    }

    /// Removes a batch from the Inbox. The downloaded files stay where they are.
    pub fn dismiss_inbox(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM inbox WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Adds each newly finished torrent's audio files to the Inbox, once per torrent.
    /// Returns the names of the new batches.
    pub fn collect_finished(&mut self, torrents: &Torrents) -> Result<Vec<String>> {
        let mut added = Vec::new();
        for status in torrents.list().into_iter().filter(|s| s.finished) {
            let inserted = self.conn.execute(
                "INSERT OR IGNORE INTO torrent_seen (info_hash) VALUES (?1)",
                [&status.info_hash],
            )?;
            if inserted == 0 {
                continue;
            }
            let audio: Vec<PathBuf> = torrents
                .files(status.id)
                .into_iter()
                .filter(|f| f.selected && is_audio(&f.path))
                .map(|f| f.location)
                .collect();
            if !audio.is_empty() {
                self.add_to_inbox("torrent", &status.name, &audio)?;
                added.push(status.name);
            }
        }
        Ok(added)
    }

    /// Where a batch's files would land under `root` with `template`. Touches nothing.
    pub fn plan_import(
        &self,
        batch_id: i64,
        root: &Path,
        template: &Template,
    ) -> Result<Vec<PlannedMove>> {
        let batch = self
            .inbox()?
            .into_iter()
            .find(|b| b.id == batch_id)
            .ok_or_else(|| Error::InvalidInput("that download is gone".into()))?;
        let mut taken: HashSet<String> = self.tracks()?.iter().map(|t| key(&t.path)).collect();
        let root = std::path::absolute(root)?;
        let mut plan = Vec::with_capacity(batch.files.len());
        for file in batch.files {
            let track = Track {
                id: 0,
                state: TrackState::Ok,
                error: None,
                meta: tags::read(&file).unwrap_or_default(),
                path: file,
            };
            let base = root.join(template.render(&track));
            let (to, collision) = free_path(&base, &taken, "");
            taken.insert(key(&to));
            plan.push(PlannedMove {
                track_id: 0,
                root: root.clone(),
                from: track.path,
                to,
                collision,
            });
        }
        Ok(plan)
    }

    /// Brings a planned batch into the library as one undoable change set.
    pub fn import_inbox(
        &mut self,
        batch_id: i64,
        plan: Vec<PlannedMove>,
        handoff: Handoff,
    ) -> Result<(i64, BatchReport)> {
        let n = plan.len();
        let description = format!("Import {n} file{}", if n == 1 { "" } else { "s" });
        let (change_set, report) = self.copy_files(&description, plan, handoff)?;
        self.conn.execute(
            "UPDATE inbox SET change_set_id = ?1 WHERE id = ?2",
            params![change_set, batch_id],
        )?;
        Ok((change_set, report))
    }
}
