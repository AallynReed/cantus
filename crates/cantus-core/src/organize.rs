use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::db::{TRACK_COLUMNS, track_from_row};
use crate::{BatchReport, Library, Result, Template, Track, TrackState};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedMove {
    pub track_id: i64,
    pub root: PathBuf,
    pub from: PathBuf,
    pub to: PathBuf,
    /// The natural target was taken, so `to` carries a numbered suffix.
    pub collision: bool,
}

impl Library {
    /// Works out where tracks go under `template` without touching anything. `ids` limits it
    /// to those tracks. Tracks already in place are left out.
    pub fn plan_organize(
        &self,
        template: &Template,
        ids: Option<&BTreeSet<i64>>,
    ) -> Result<Vec<PlannedMove>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {TRACK_COLUMNS}, (SELECT path FROM root WHERE id = root_id) AS root_path
             FROM track ORDER BY id"
        ))?;
        let tracks: Vec<(Track, PathBuf)> = stmt
            .query_map([], |r| {
                Ok((
                    track_from_row(r)?,
                    PathBuf::from(r.get::<_, String>("root_path")?),
                ))
            })?
            .collect::<Result<_, _>>()?;

        let mut taken: HashSet<String> = tracks.iter().map(|(t, _)| key(&t.path)).collect();
        let mut moves = Vec::new();
        for (track, root) in &tracks {
            let wanted = ids.is_none_or(|ids| ids.contains(&track.id));
            if track.state != TrackState::Ok || !wanted {
                continue;
            }
            let own = key(&track.path);
            taken.remove(&own);
            let base = root.join(template.render(track));
            let (to, collision) = free_path(&base, &taken, &own);
            taken.insert(key(&to));
            if to != track.path {
                moves.push(PlannedMove {
                    track_id: track.id,
                    root: root.clone(),
                    from: track.path.clone(),
                    to,
                    collision,
                });
            }
        }
        Ok(moves)
    }

    /// Applies a plan as one undoable change set.
    pub fn organize(&mut self, moves: Vec<PlannedMove>) -> Result<(i64, BatchReport)> {
        let n = moves.len();
        let description = format!("Organize {n} file{}", if n == 1 { "" } else { "s" });
        self.move_files(&description, moves)
    }
}

/// Paths compare case-insensitively: two names differing only by case collide on Windows
/// and macOS, and a library must stay portable.
pub(crate) fn key(path: &Path) -> String {
    path.to_string_lossy().to_lowercase()
}

/// `base`, or `base (2)`, `base (3)`... The track's own current path always counts as free,
/// so organizing twice changes nothing.
pub(crate) fn free_path(base: &Path, taken: &HashSet<String>, own: &str) -> (PathBuf, bool) {
    let mut n = 1;
    loop {
        let candidate = if n == 1 {
            base.to_path_buf()
        } else {
            numbered(base, n)
        };
        let k = key(&candidate);
        if !taken.contains(&k) && (k == own || fs::symlink_metadata(&candidate).is_err()) {
            return (candidate, n > 1);
        }
        n += 1;
    }
}

fn numbered(base: &Path, n: u32) -> PathBuf {
    let stem = base.file_stem().unwrap_or_default().to_string_lossy();
    let name = match base.extension() {
        Some(ext) => format!("{stem} ({n}).{}", ext.to_string_lossy()),
        None => format!("{stem} ({n})"),
    };
    base.with_file_name(name)
}
