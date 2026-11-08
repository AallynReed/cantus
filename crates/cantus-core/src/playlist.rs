//! Playlists, stored in the library and shared as plain `.m3u8` or JSON files.
//!
//! Files on disk are only read when importing and written when exporting to a path the user
//! chose. Paths inside a playlist are matched against the library and never opened.

use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use rusqlite::{Transaction, params};
use serde::{Deserialize, Serialize};

use crate::journal::write_file;
use crate::{Error, Library, Result, Track};

const FORMAT: &str = "cantus-playlist/1";
const MAX_PLAYLIST_BYTES: u64 = 64 * 1024 * 1024;
const FUZZY_DURATION_MS: u32 = 3_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaylistSummary {
    pub id: i64,
    pub name: String,
    pub entries: usize,
}

/// One playlist line. `track_id` is `None` when the track is not in this library; the other
/// fields are what is known about it and are kept so nothing is lost.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    #[serde(skip)]
    pub track_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_mbid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PlaylistFile {
    format: String,
    name: String,
    entries: Vec<Entry>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ImportReport {
    pub playlist_id: i64,
    pub resolved: usize,
    pub unresolved: usize,
}

impl Library {
    pub fn create_playlist(&self, name: &str) -> Result<i64> {
        self.conn
            .execute("INSERT INTO playlist (name) VALUES (?1)", [name])?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn delete_playlist(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM playlist WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn playlists(&self) -> Result<Vec<PlaylistSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.id, p.name, COUNT(e.pos) FROM playlist p
             LEFT JOIN playlist_entry e ON e.playlist_id = p.id
             GROUP BY p.id ORDER BY p.name COLLATE NOCASE, p.id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(PlaylistSummary {
                id: r.get(0)?,
                name: r.get(1)?,
                entries: r.get::<_, i64>(2)? as usize,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// A playlist's entries in order. Details of tracks still in the library are current.
    pub fn playlist_entries(&self, id: i64) -> Result<Vec<Entry>> {
        let mut stmt = self.conn.prepare(
            "SELECT e.track_id, COALESCE(t.path, e.path),
                 COALESCE(t.recording_mbid, e.recording_mbid), COALESCE(t.artist, e.artist),
                 COALESCE(t.title, e.title), COALESCE(t.duration_ms, e.duration_ms)
             FROM playlist_entry e LEFT JOIN track t ON t.id = e.track_id
             WHERE e.playlist_id = ?1 ORDER BY e.pos",
        )?;
        let rows = stmt.query_map([id], |r| {
            Ok(Entry {
                track_id: r.get(0)?,
                path: r.get(1)?,
                recording_mbid: r.get(2)?,
                artist: r.get(3)?,
                title: r.get(4)?,
                duration_ms: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Replaces a playlist's contents, for removing and reordering.
    pub fn set_playlist_entries(&mut self, id: i64, entries: &[Entry]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM playlist_entry WHERE playlist_id = ?1", [id])?;
        insert_entries(&tx, id, 0, entries)?;
        tx.commit()?;
        Ok(())
    }

    pub fn add_to_playlist(&mut self, id: i64, track_ids: &[i64]) -> Result<()> {
        let tracks: HashMap<i64, Track> = self.tracks()?.into_iter().map(|t| (t.id, t)).collect();
        let entries: Vec<Entry> = track_ids
            .iter()
            .filter_map(|track_id| tracks.get(track_id))
            .map(entry_for)
            .collect();
        let tx = self.conn.transaction()?;
        let next: i64 = tx.query_row(
            "SELECT COALESCE(MAX(pos) + 1, 0) FROM playlist_entry WHERE playlist_id = ?1",
            [id],
            |r| r.get(0),
        )?;
        insert_entries(&tx, id, next, &entries)?;
        tx.commit()?;
        Ok(())
    }

    /// Writes `.json` as a Cantus playlist and anything else as `.m3u8`.
    pub fn export_playlist(&self, id: i64, file: &Path) -> Result<()> {
        let name = self
            .playlists()?
            .into_iter()
            .find(|p| p.id == id)
            .ok_or_else(|| Error::InvalidInput("that playlist no longer exists".into()))?
            .name;
        let base = file.parent().unwrap_or(Path::new(""));
        let entries: Vec<Entry> = self
            .playlist_entries(id)?
            .into_iter()
            .map(|mut entry| {
                entry.path = entry.path.and_then(|p| shareable_path(&p, base));
                entry
            })
            .collect();
        let text = if is_json(file) {
            let playlist = PlaylistFile {
                format: FORMAT.into(),
                name,
                entries,
            };
            serde_json::to_string_pretty(&playlist)
                .map_err(|e| Error::InvalidInput(e.to_string()))?
        } else {
            write_m3u8(&name, &entries)
        };
        write_file(file, text.as_bytes())?;
        Ok(())
    }

    /// Imports a playlist file, matching each entry against the library. Entries with no
    /// match are kept and counted, never dropped.
    pub fn import_playlist(&mut self, file: &Path) -> Result<ImportReport> {
        if fs::metadata(file)?.len() > MAX_PLAYLIST_BYTES {
            return Err(Error::InvalidInput(
                "that playlist file is too large".into(),
            ));
        }
        let text = fs::read_to_string(file)?;
        let (name, mut entries) = if is_json(file) {
            let playlist: PlaylistFile = serde_json::from_str(text.trim_start_matches('\u{feff}'))
                .map_err(|e| Error::InvalidInput(format!("not a Cantus playlist: {e}")))?;
            if playlist.format != FORMAT {
                let found = playlist.format;
                return Err(Error::InvalidInput(format!(
                    "unknown playlist format: {found}"
                )));
            }
            (Some(playlist.name), playlist.entries)
        } else {
            parse_m3u8(&text)
        };
        let name = name.unwrap_or_else(|| {
            let stem = file.file_stem().unwrap_or_default();
            stem.to_string_lossy().into_owned()
        });

        let tracks = self.tracks()?;
        let index = Index::new(&tracks);
        let base = std::path::absolute(file)?;
        let base = base.parent().unwrap_or(Path::new(""));
        for entry in &mut entries {
            entry.track_id = index.resolve(entry, base);
        }
        let resolved = entries.iter().filter(|e| e.track_id.is_some()).count();
        let unresolved = entries.len() - resolved;

        let tx = self.conn.transaction()?;
        tx.execute("INSERT INTO playlist (name) VALUES (?1)", [&name])?;
        let playlist_id = tx.last_insert_rowid();
        insert_entries(&tx, playlist_id, 0, &entries)?;
        tx.commit()?;
        Ok(ImportReport {
            playlist_id,
            resolved,
            unresolved,
        })
    }
}

fn insert_entries(tx: &Transaction, id: i64, first: i64, entries: &[Entry]) -> Result<()> {
    let mut stmt = tx.prepare(
        "INSERT INTO playlist_entry
             (playlist_id, pos, track_id, path, recording_mbid, artist, title, duration_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?;
    for (pos, e) in (first..).zip(entries) {
        stmt.execute(params![
            id,
            pos,
            e.track_id,
            e.path,
            e.recording_mbid,
            e.artist,
            e.title,
            e.duration_ms
        ])?;
    }
    Ok(())
}

fn entry_for(track: &Track) -> Entry {
    Entry {
        track_id: Some(track.id),
        path: track.path.to_str().map(str::to_owned),
        recording_mbid: track.meta.recording_mbid.clone(),
        artist: track.meta.artist.clone(),
        title: track.meta.title.clone(),
        duration_ms: track.meta.duration_ms,
    }
}

fn is_json(file: &Path) -> bool {
    file.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("json"))
}

/// Finds library tracks by path, MusicBrainz id, then artist, title and duration.
struct Index<'a> {
    by_path: HashMap<String, i64>,
    by_mbid: HashMap<&'a str, i64>,
    by_name: HashMap<(String, String), Vec<&'a Track>>,
}

impl<'a> Index<'a> {
    fn new(tracks: &'a [Track]) -> Self {
        let mut index = Self {
            by_path: HashMap::with_capacity(tracks.len()),
            by_mbid: HashMap::new(),
            by_name: HashMap::new(),
        };
        for track in tracks {
            index.by_path.insert(path_key(&track.path), track.id);
            if let Some(mbid) = track.meta.recording_mbid.as_deref() {
                index.by_mbid.entry(mbid).or_insert(track.id);
            }
            if let (Some(artist), Some(title)) = (&track.meta.artist, &track.meta.title) {
                index
                    .by_name
                    .entry((artist.to_lowercase(), title.to_lowercase()))
                    .or_default()
                    .push(track);
            }
        }
        index
    }

    fn resolve(&self, entry: &Entry, base: &Path) -> Option<i64> {
        let by_path = entry.path.as_deref().and_then(|path| {
            let path = path.replace('\\', "/");
            self.by_path
                .get(&path_key(&normalize(&base.join(path))))
                .copied()
        });
        let by_mbid = || self.by_mbid.get(entry.recording_mbid.as_deref()?).copied();
        by_path.or_else(by_mbid).or_else(|| self.by_details(entry))
    }

    fn by_details(&self, entry: &Entry) -> Option<i64> {
        let key = (
            entry.artist.as_ref()?.to_lowercase(),
            entry.title.as_ref()?.to_lowercase(),
        );
        let close = |t: &&&Track| match (t.meta.duration_ms, entry.duration_ms) {
            (Some(a), Some(b)) => a.abs_diff(b) <= FUZZY_DURATION_MS,
            _ => true,
        };
        self.by_name.get(&key)?.iter().find(close).map(|t| t.id)
    }
}

/// Windows and macOS compare names without case by default.
fn path_key(path: &Path) -> String {
    let text = path.to_string_lossy();
    if cfg!(any(windows, target_os = "macos")) {
        text.to_lowercase()
    } else {
        text.into_owned()
    }
}

/// Resolves `.` and `..` without touching the disk.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            part => out.push(part),
        }
    }
    out
}

/// The path to put in a shared file. Absolute paths reveal the user's folders, so a track
/// on another drive gets no path and is found by its other details.
fn shareable_path(path: &str, base: &Path) -> Option<String> {
    let target = Path::new(path);
    if !target.is_absolute() {
        return Some(path.to_owned());
    }
    let base = std::path::absolute(base).ok()?;
    let mut target_parts = target.components().peekable();
    let mut base_parts = base.components().peekable();
    while target_parts.peek().is_some() && target_parts.peek() == base_parts.peek() {
        target_parts.next();
        base_parts.next();
    }
    if matches!(
        target_parts.peek(),
        Some(Component::Prefix(_) | Component::RootDir)
    ) {
        return None;
    }
    let ups = base_parts.count();
    let down = target_parts.map(|c| c.as_os_str().to_string_lossy().into_owned());
    let parts: Vec<String> = std::iter::repeat_n("..".to_owned(), ups)
        .chain(down)
        .collect();
    Some(parts.join("/"))
}

/// Control characters would let a tag value forge extra playlist lines.
fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn write_m3u8(name: &str, entries: &[Entry]) -> String {
    let mut out = format!("#EXTM3U\n#PLAYLIST:{}\n", one_line(name));
    for entry in entries {
        let Some(path) = &entry.path else { continue };
        let seconds = entry.duration_ms.map_or(-1, |ms| i64::from(ms / 1000));
        let label = match (&entry.artist, &entry.title) {
            (Some(artist), Some(title)) => format!("{artist} - {title}"),
            (None, Some(title)) => title.clone(),
            (Some(artist), None) => format!("{artist} - "),
            (None, None) => String::new(),
        };
        out += &format!(
            "#EXTINF:{seconds},{}\n{}\n",
            one_line(&label),
            one_line(path)
        );
    }
    out
}

fn parse_m3u8(text: &str) -> (Option<String>, Vec<Entry>) {
    let mut name = None;
    let mut entries = Vec::new();
    let mut pending = Entry::default();
    for line in text.trim_start_matches('\u{feff}').lines().map(str::trim) {
        if let Some(info) = line.strip_prefix("#EXTINF:") {
            pending = parse_extinf(info);
        } else if let Some(value) = line.strip_prefix("#PLAYLIST:") {
            name = Some(value.to_owned());
        } else if !line.is_empty() && !line.starts_with('#') {
            pending.path = Some(line.to_owned());
            entries.push(std::mem::take(&mut pending));
        }
    }
    (name, entries)
}

fn parse_extinf(info: &str) -> Entry {
    let (seconds, label) = info.split_once(',').unwrap_or((info, ""));
    let duration_ms = seconds
        .trim()
        .parse::<u32>()
        .ok()
        .and_then(|s| s.checked_mul(1000));
    let (artist, title) = match label.split_once(" - ") {
        Some((artist, title)) => (Some(artist.to_owned()), Some(title.to_owned())),
        None => (None, Some(label.to_owned())),
    };
    let present = |s: Option<String>| s.filter(|s| !s.is_empty());
    Entry {
        duration_ms,
        artist: present(artist),
        title: present(title),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn entry_strategy() -> impl Strategy<Value = Entry> {
        (
            "[a-zA-Z0-9é_ /.-]{0,30}[a-zA-Z0-9é_]",
            proptest::option::of("[a-zA-Z0-9é,_-][a-zA-Z0-9é,_ -]{0,15}[a-zA-Z0-9é,_-]"),
            "[a-zA-Z0-9é,_-][a-zA-Z0-9é,_ -]{0,15}[a-zA-Z0-9é,_-]",
            proptest::option::of(0u32..100_000),
        )
            .prop_filter("the separator is ambiguous", |(path, artist, title, _)| {
                let clean = |s: &str| !s.contains(" -") && !s.contains("- ");
                clean(title)
                    && artist.as_deref().is_none_or(clean)
                    && !path.trim_start().starts_with('#')
                    && !path.starts_with(' ')
            })
            .prop_map(|(path, artist, title, seconds)| Entry {
                track_id: None,
                path: Some(path),
                recording_mbid: None,
                artist,
                title: Some(title),
                duration_ms: seconds.map(|s| s * 1000),
            })
    }

    proptest! {
        #[test]
        fn m3u8_round_trips(entries in proptest::collection::vec(entry_strategy(), 0..20)) {
            let (name, parsed) = parse_m3u8(&write_m3u8("My List", &entries));
            prop_assert_eq!(name.as_deref(), Some("My List"));
            prop_assert_eq!(parsed, entries);
        }

        #[test]
        fn json_round_trips(
            mut entries in proptest::collection::vec(entry_strategy(), 0..20),
            mbid in proptest::option::of("[a-f0-9-]{1,36}"),
        ) {
            if let Some(first) = entries.first_mut() {
                first.recording_mbid = mbid;
            }
            let file = PlaylistFile { format: FORMAT.into(), name: "x".into(), entries: entries.clone() };
            let parsed: PlaylistFile =
                serde_json::from_str(&serde_json::to_string_pretty(&file).unwrap()).unwrap();
            prop_assert_eq!(parsed.entries, entries);
        }

        #[test]
        fn parsing_arbitrary_text_never_panics(text: String) {
            parse_m3u8(&text);
        }
    }

    #[test]
    fn a_tag_value_cannot_forge_playlist_lines() {
        let entries = [Entry {
            path: Some("a.flac".into()),
            title: Some("evil\n/etc/passwd\r\n#EXTINF:1,x".into()),
            ..Default::default()
        }];
        let (_, parsed) = parse_m3u8(&write_m3u8("n\nm", &entries));
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].path.as_deref(), Some("a.flac"));
    }

    #[test]
    fn foreign_m3u_files_parse() {
        let text = "\u{feff}#EXTM3U\r\n#EXTINF:-1,Just A Title\r\nmusic\\song one.mp3\r\n\
                    # a comment\r\nhttp://example.com/stream.mp3\r\n";
        let (name, entries) = parse_m3u8(text);
        assert_eq!(name, None);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].title.as_deref(), Some("Just A Title"));
        assert_eq!(entries[0].duration_ms, None);
        assert_eq!(entries[0].path.as_deref(), Some("music\\song one.mp3"));
    }

    #[test]
    fn paths_are_made_relative_to_the_playlist() {
        let root = std::path::absolute("/music").unwrap();
        let track = root.join("a/b/song.flac");
        let shared = |base: &str| shareable_path(track.to_str().unwrap(), &root.join(base));
        assert_eq!(shared("a").as_deref(), Some("b/song.flac"));
        assert_eq!(shared("a/c").as_deref(), Some("../b/song.flac"));
        assert_eq!(shared("x/y").as_deref(), Some("../../a/b/song.flac"));
    }

    #[test]
    fn normalize_never_touches_the_disk_or_climbs_past_the_root() {
        let root = std::path::absolute("/").unwrap();
        assert_eq!(normalize(&root.join("a/../b/./c")), root.join("b/c"));
        assert_eq!(normalize(&root.join("../../x")), root.join("x"));
    }
}
