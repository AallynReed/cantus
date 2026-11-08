//! Turns tracks into ranked MusicBrainz candidates.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use rusqlite::params;
use serde::Deserialize;

use crate::fingerprint::fingerprint;
use crate::{Field, Library, Lookup, Metadata, Result, Track};

const MIN_ACOUSTID_SCORE: u8 = 50;
const MAX_RECORDINGS_PER_TRACK: usize = 3;
const MAX_CANDIDATES: usize = 10;
const CACHE_TTL_SECS: i64 = 30 * 24 * 60 * 60;
const DURATION_TOLERANCE_MS: u32 = 5_000;
const RECORDING_INCLUDES: &str = "artist-credits+releases+media";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub recording_mbid: String,
    pub release_mbid: String,
    pub title: String,
    pub artist: String,
    pub album_artist: String,
    pub album: String,
    pub track_no: Option<u32>,
    pub disc_no: Option<u32>,
    pub year: Option<u16>,
    /// 0 to 100.
    pub score: u8,
    /// Matched by audio rather than by the file's existing tags.
    pub by_fingerprint: bool,
    official: bool,
}

/// One field a candidate would change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldDiff {
    pub field: Field,
    pub old: Option<String>,
    pub new: String,
}

impl Candidate {
    fn value(&self, field: Field) -> Option<String> {
        let text = |s: &String| Some(s.clone());
        match field {
            Field::Title => text(&self.title),
            Field::Artist => text(&self.artist),
            Field::AlbumArtist => text(&self.album_artist),
            Field::Album => text(&self.album),
            Field::TrackNo => self.track_no.map(|n| n.to_string()),
            Field::DiscNo => self.disc_no.map(|n| n.to_string()),
            Field::Year => self.year.map(|n| n.to_string()),
            Field::Genre => None,
            Field::RecordingMbid => text(&self.recording_mbid),
            Field::ReleaseMbid => text(&self.release_mbid),
        }
    }

    /// The fields that would change on a file holding `current`.
    pub fn diff(&self, current: &Metadata) -> Vec<FieldDiff> {
        Field::IDENTIFIED
            .into_iter()
            .filter_map(|field| {
                let new = self.value(field).filter(|v| !v.is_empty())?;
                let old = field.get(current);
                (old.as_deref() != Some(new.as_str())).then_some(FieldDiff { field, old, new })
            })
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identified {
    pub track_id: i64,
    pub candidates: Vec<Candidate>,
    /// Why there are no candidates, when something went wrong.
    pub error: Option<String>,
}

#[derive(Deserialize)]
struct Recording {
    id: String,
    title: String,
    #[serde(rename = "artist-credit", default)]
    artist_credit: Vec<Credit>,
    #[serde(default)]
    releases: Vec<Release>,
    #[serde(default)]
    score: Option<u8>,
}

#[derive(Deserialize)]
struct Credit {
    name: String,
    #[serde(default)]
    joinphrase: String,
}

#[derive(Deserialize)]
struct Release {
    id: String,
    title: String,
    #[serde(default)]
    date: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(rename = "artist-credit", default)]
    artist_credit: Vec<Credit>,
    #[serde(default)]
    media: Vec<Medium>,
}

#[derive(Deserialize)]
struct Medium {
    #[serde(default)]
    position: Option<u32>,
    // Lookups say "tracks", searches say "track".
    #[serde(default, alias = "track")]
    tracks: Vec<ReleaseTrack>,
}

#[derive(Deserialize)]
struct ReleaseTrack {
    #[serde(default)]
    position: Option<u32>,
    #[serde(default)]
    number: Option<String>,
}

#[derive(Deserialize)]
struct SearchResults {
    #[serde(default)]
    recordings: Vec<Recording>,
}

fn credit_text(credits: &[Credit]) -> String {
    credits
        .iter()
        .map(|c| format!("{}{}", c.name, c.joinphrase))
        .collect()
}

fn candidates_from(recording: Recording, score: u8, by_fingerprint: bool) -> Vec<Candidate> {
    let artist = credit_text(&recording.artist_credit);
    recording
        .releases
        .iter()
        .map(|release| {
            let position = release.media.iter().find_map(|medium| {
                let track = medium.tracks.first()?;
                let number = track
                    .position
                    .or_else(|| track.number.as_deref()?.parse().ok());
                Some((number, medium.position))
            });
            let album_artist = credit_text(&release.artist_credit);
            Candidate {
                recording_mbid: recording.id.clone(),
                release_mbid: release.id.clone(),
                title: recording.title.clone(),
                artist: artist.clone(),
                album_artist: if album_artist.is_empty() {
                    artist.clone()
                } else {
                    album_artist
                },
                album: release.title.clone(),
                track_no: position.and_then(|p| p.0),
                disc_no: position.and_then(|p| p.1),
                year: release
                    .date
                    .as_deref()
                    .and_then(|d| d.get(..4)?.parse().ok()),
                score,
                by_fingerprint,
                official: release.status.as_deref() == Some("Official"),
            }
        })
        .collect()
}

/// Best first: score, official releases, the artist's own releases over compilations, the
/// album the file already claims, earliest year.
fn rank(candidates: &mut Vec<Candidate>, current: &Metadata) {
    let album = current.album.as_deref().unwrap_or_default().to_lowercase();
    candidates.sort_by_key(|c| {
        (
            std::cmp::Reverse(c.score),
            !c.official,
            c.album_artist != c.artist,
            c.album.to_lowercase() != album,
            c.year.unwrap_or(u16::MAX),
        )
    });
    candidates.truncate(MAX_CANDIDATES);
}

/// Tracks in one folder are usually one album, so a release shared by several of them moves
/// to the front for each, keeping an album from scattering across editions.
fn prefer_shared_release(results: &mut [Identified], tracks: &HashMap<i64, &Track>) {
    let mut folders: HashMap<&Path, Vec<usize>> = HashMap::new();
    for (i, result) in results.iter().enumerate() {
        if let Some(folder) = tracks[&result.track_id].path.parent() {
            folders.entry(folder).or_default().push(i);
        }
    }
    for members in folders.into_values().filter(|m| m.len() > 1) {
        let mut votes: HashMap<String, usize> = HashMap::new();
        for &i in &members {
            let releases: HashSet<_> = results[i]
                .candidates
                .iter()
                .map(|c| &c.release_mbid)
                .collect();
            for release in releases {
                *votes.entry(release.clone()).or_default() += 1;
            }
        }
        let best = votes
            .into_iter()
            .filter(|(_, n)| *n > 1)
            .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)));
        if let Some((release, _)) = best {
            for &i in &members {
                results[i]
                    .candidates
                    .sort_by_key(|c| c.release_mbid != release);
            }
        }
    }
}

fn lucene_phrase(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

fn search_query(meta: &Metadata) -> Option<String> {
    let title = meta.title.as_deref().filter(|t| !t.trim().is_empty())?;
    let mut query = format!("recording:{}", lucene_phrase(title));
    if let Some(artist) = meta.artist.as_deref().filter(|a| !a.trim().is_empty()) {
        query += &format!(" AND artist:{}", lucene_phrase(artist));
    }
    if let Some(ms) = meta.duration_ms.filter(|ms| *ms > DURATION_TOLERANCE_MS) {
        let (low, high) = (ms - DURATION_TOLERANCE_MS, ms + DURATION_TOLERANCE_MS);
        query += &format!(" AND dur:[{low} TO {high}]");
    }
    Some(query)
}

impl Library {
    /// Looks up candidates for `ids`. `progress(done, total)` returning false stops early.
    /// Nothing is written to the files.
    pub fn identify(
        &self,
        lookup: &mut Lookup,
        ids: &BTreeSet<i64>,
        mut progress: impl FnMut(usize, usize) -> bool,
    ) -> Result<Vec<Identified>> {
        let tracks = self.tracks()?;
        let selected: Vec<&Track> = tracks.iter().filter(|t| ids.contains(&t.id)).collect();
        let by_id: HashMap<i64, &Track> = selected.iter().map(|t| (t.id, *t)).collect();

        let mut results = Vec::with_capacity(selected.len());
        for (done, track) in selected.iter().enumerate() {
            if !progress(done, selected.len()) {
                break;
            }
            results.push(self.identify_one(lookup, track));
        }
        prefer_shared_release(&mut results, &by_id);
        Ok(results)
    }

    fn identify_one(&self, lookup: &mut Lookup, track: &Track) -> Identified {
        let mut error = None;
        let mut candidates = Vec::new();

        if lookup.has_acoustid_key() {
            match self.by_fingerprint(lookup, track) {
                Ok(found) => candidates = found,
                Err(reason) => error = Some(reason),
            }
        }
        if candidates.is_empty() {
            match self.by_text(lookup, &track.meta) {
                Ok(found) => {
                    candidates = found;
                    if !candidates.is_empty() {
                        error = None;
                    }
                }
                Err(reason) => error = error.or(Some(reason)),
            }
        }
        rank(&mut candidates, &track.meta);
        Identified {
            track_id: track.id,
            candidates,
            error,
        }
    }

    fn by_fingerprint(&self, lookup: &mut Lookup, track: &Track) -> Result<Vec<Candidate>, String> {
        let print = fingerprint(&track.path)?;
        let secs = track.meta.duration_ms.unwrap_or(0) / 1000;
        let key = format!(
            "acoustid:{secs}:{}",
            blake3::hash(print.as_bytes()).to_hex()
        );
        let matches = match self.cache_get(&key) {
            Some(body) => serde_json::from_str(&body).map_err(|e| e.to_string())?,
            None => {
                let matches = lookup.acoustid(&print, secs)?;
                if let Ok(body) = serde_json::to_string(&matches) {
                    self.cache_put(&key, &body);
                }
                matches
            }
        };

        let mut seen = HashSet::new();
        let mut candidates = Vec::new();
        for (id, score) in matches
            .into_iter()
            .filter(|(id, score): &(String, u8)| {
                *score >= MIN_ACOUSTID_SCORE && seen.insert(id.clone())
            })
            .take(MAX_RECORDINGS_PER_TRACK)
        {
            let path = format!("recording/{id}");
            let body = self.cached_musicbrainz(lookup, &path, &[("inc", RECORDING_INCLUDES)])?;
            if let Some(body) = body {
                let recording: Recording =
                    serde_json::from_str(&body).map_err(|e| e.to_string())?;
                candidates.extend(candidates_from(recording, score, true));
            }
        }
        Ok(candidates)
    }

    fn by_text(&self, lookup: &mut Lookup, meta: &Metadata) -> Result<Vec<Candidate>, String> {
        let Some(query) = search_query(meta) else {
            return Err("no title to search for".into());
        };
        let params = [("query", query.as_str()), ("limit", "25")];
        let Some(body) = self.cached_musicbrainz(lookup, "recording", &params)? else {
            return Ok(Vec::new());
        };
        let results: SearchResults = serde_json::from_str(&body).map_err(|e| e.to_string())?;
        Ok(results
            .recordings
            .into_iter()
            .flat_map(|r| {
                let score = r.score.unwrap_or(0);
                candidates_from(r, score, false)
            })
            .collect())
    }

    fn cached_musicbrainz(
        &self,
        lookup: &mut Lookup,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<Option<String>, String> {
        let key = format!("mb:{path}?{query:?}");
        if let Some(body) = self.cache_get(&key) {
            return Ok(Some(body));
        }
        let Some(bytes) = lookup.musicbrainz(path, query)? else {
            return Ok(None);
        };
        let body = String::from_utf8(bytes).map_err(|e| e.to_string())?;
        self.cache_put(&key, &body);
        Ok(Some(body))
    }

    /// The cache only saves requests, so its errors count as misses.
    fn cache_get(&self, key: &str) -> Option<String> {
        self.conn
            .query_row(
                "SELECT body FROM lookup_cache
                 WHERE key = ?1 AND fetched_at > unixepoch() - ?2",
                params![key, CACHE_TTL_SECS],
                |r| r.get(0),
            )
            .ok()
    }

    fn cache_put(&self, key: &str, body: &str) {
        let _ = self.conn.execute(
            "INSERT INTO lookup_cache (key, body, fetched_at) VALUES (?1, ?2, unixepoch())
             ON CONFLICT (key) DO UPDATE
             SET body = excluded.body, fetched_at = excluded.fetched_at",
            params![key, body],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lookup::{Endpoints, fake};
    use crate::{TagChange, TrackEdit};
    use std::fs;

    const RECORDING: &str = r#"{
        "id": "rec-1", "title": "Real Title",
        "artist-credit": [{"name": "Real Artist", "joinphrase": " feat. "}, {"name": "Guest"}],
        "releases": [
            {"id": "rel-bootleg", "title": "Bootleg", "status": "Bootleg", "date": "1990",
             "media": [{"position": 1, "tracks": [{"position": 1, "number": "1"}]}]},
            {"id": "rel-1", "title": "Real Album", "status": "Official", "date": "2001-05-21",
             "artist-credit": [{"name": "Album Artist"}],
             "media": [{"position": 2, "tracks": [{"position": 7, "number": "7"}]}]}
        ]}"#;
    const SEARCH: &str = r#"{"recordings": [{
        "id": "rec-2", "score": 88, "title": "Found By Text",
        "artist-credit": [{"name": "Text Artist"}],
        "releases": [{"id": "rel-2", "title": "Text Album", "status": "Official",
            "media": [{"position": 1, "track": [{"number": "4"}]}]}]}]}"#;

    fn fixture(name: &str) -> std::path::PathBuf {
        crate::test_fixtures().join(name)
    }

    fn respond(request: &str) -> (u16, Vec<u8>) {
        if request.starts_with("POST /acoustid") {
            let body =
                r#"{"status":"ok","results":[{"score":0.97,"recordings":[{"id":"rec-1"}]}]}"#;
            (200, body.into())
        } else if request.contains("/recording/rec-1") {
            (200, RECORDING.into())
        } else if request.contains("/recording?") {
            (200, SEARCH.into())
        } else if request.contains("/front-500") {
            (404, vec![])
        } else {
            (500, vec![])
        }
    }

    fn lookup(server: &fake::Server, key: Option<&str>) -> Lookup {
        let endpoints = Endpoints {
            acoustid: format!("{}/acoustid", server.url),
            musicbrainz: server.url.clone(),
            cover_art: server.url.clone(),
        };
        Lookup::new(endpoints, key.map(str::to_owned)).unwrap()
    }

    fn library_with(names: &[(&str, &str)]) -> (tempfile::TempDir, Library, BTreeSet<i64>) {
        let dir = tempfile::tempdir().unwrap();
        for (fixture_name, dest) in names {
            let dest = dir.path().join(dest);
            fs::create_dir_all(dest.parent().unwrap()).unwrap();
            fs::copy(fixture(fixture_name), dest).unwrap();
        }
        let mut lib = Library::open_in_memory().unwrap();
        lib.scan(dir.path()).unwrap();
        let ids = lib.tracks().unwrap().iter().map(|t| t.id).collect();
        (dir, lib, ids)
    }

    #[test]
    fn fingerprint_match_builds_ranked_candidates() {
        let server = fake::start(respond);
        let (_dir, lib, ids) = library_with(&[("long.flac", "a.flac")]);

        let results = lib
            .identify(&mut lookup(&server, Some("k")), &ids, |_, _| true)
            .unwrap();

        let c = &results[0].candidates;
        assert_eq!(results[0].error, None);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].release_mbid, "rel-1", "official release ranks first");
        assert_eq!(c[0].artist, "Real Artist feat. Guest");
        assert_eq!(c[0].album_artist, "Album Artist");
        assert_eq!(
            (c[0].track_no, c[0].disc_no, c[0].year),
            (Some(7), Some(2), Some(2001))
        );
        assert_eq!(c[0].score, 97);
        assert!(c[0].by_fingerprint);
        assert_eq!(
            c[1].album_artist, "Real Artist feat. Guest",
            "falls back to the artist"
        );
    }

    #[test]
    fn accepting_a_candidate_writes_tags_and_undo_restores_the_file() {
        let server = fake::start(respond);
        let (dir, mut lib, ids) = library_with(&[("long.flac", "a.flac")]);
        let path = dir.path().join("a.flac");
        let original = fs::read(&path).unwrap();
        let results = lib
            .identify(&mut lookup(&server, Some("k")), &ids, |_, _| true)
            .unwrap();
        let track = lib.tracks().unwrap().remove(0);

        let changes = results[0].candidates[0]
            .diff(&track.meta)
            .into_iter()
            .map(|d| TagChange {
                field: d.field,
                value: Some(d.new),
            })
            .collect();
        let edit = TrackEdit {
            track_id: track.id,
            changes,
            cover: None,
        };
        let (change_set, report) = lib.apply_edits("Identify 1 track", vec![edit]).unwrap();

        assert!(report.failed.is_empty(), "{:?}", report.failed);
        let m = lib.tracks().unwrap().remove(0).meta;
        assert_eq!(m.title.as_deref(), Some("Real Title"));
        assert_eq!(m.album_artist.as_deref(), Some("Album Artist"));
        assert_eq!(m.track_no, Some(7));
        assert_eq!(m.recording_mbid.as_deref(), Some("rec-1"));
        assert_eq!(m.release_mbid.as_deref(), Some("rel-1"));
        assert!(
            results[0].candidates[0].diff(&m).is_empty(),
            "applied means no more changes"
        );

        lib.undo(change_set).unwrap();
        assert!(fs::read(&path).unwrap() == original);
    }

    #[test]
    fn without_a_key_it_searches_by_tags_with_the_duration() {
        let server = fake::start(respond);
        let (_dir, lib, ids) = library_with(&[("long.flac", "a.flac")]);

        let results = lib
            .identify(&mut lookup(&server, None), &ids, |_, _| true)
            .unwrap();

        let c = &results[0].candidates[0];
        assert_eq!(
            (c.title.as_str(), c.score, c.track_no),
            ("Found By Text", 88, Some(4))
        );
        assert!(!c.by_fingerprint);
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0].contains("recording%3A%22Test+Tone%22"),
            "{}",
            requests[0]
        );
    }

    #[test]
    fn undecodable_audio_falls_back_to_text_search() {
        let server = fake::start(respond);
        let (_dir, lib, ids) = library_with(&[("tone.opus", "a.opus")]);
        let results = lib
            .identify(&mut lookup(&server, Some("k")), &ids, |_, _| true)
            .unwrap();
        assert_eq!(results[0].candidates[0].title, "Found By Text");
        assert_eq!(results[0].error, None);
    }

    #[test]
    fn repeat_lookups_come_from_the_cache() {
        let server = fake::start(respond);
        let (_dir, lib, ids) = library_with(&[("long.flac", "a.flac")]);
        let mut client = lookup(&server, Some("k"));

        let first = lib.identify(&mut client, &ids, |_, _| true).unwrap();
        let sent = server.requests.lock().unwrap().len();
        let second = lib.identify(&mut client, &ids, |_, _| true).unwrap();

        assert_eq!(sent, 2);
        assert_eq!(server.requests.lock().unwrap().len(), sent);
        assert_eq!(first, second);
    }

    #[test]
    fn network_failure_is_reported_per_track() {
        let server = fake::start(|_| (500, vec![]));
        let (_dir, lib, ids) = library_with(&[("long.flac", "a.flac")]);
        let results = lib
            .identify(&mut lookup(&server, None), &ids, |_, _| true)
            .unwrap();
        assert!(results[0].candidates.is_empty());
        assert!(results[0].error.as_deref().unwrap().contains("500"));
    }

    #[test]
    fn progress_can_stop_the_batch() {
        let server = fake::start(respond);
        let (_dir, lib, ids) = library_with(&[("tone.flac", "a.flac"), ("tone.mp3", "b.mp3")]);
        let results = lib
            .identify(&mut lookup(&server, None), &ids, |done, _| done < 1)
            .unwrap();
        assert_eq!(results.len(), 1);
    }

    fn candidate(release: &str, score: u8) -> Candidate {
        Candidate {
            recording_mbid: "r".into(),
            release_mbid: release.into(),
            title: "t".into(),
            artist: "a".into(),
            album_artist: "a".into(),
            album: release.into(),
            track_no: None,
            disc_no: None,
            year: None,
            score,
            by_fingerprint: true,
            official: true,
        }
    }

    #[test]
    fn tracks_in_one_folder_converge_on_a_shared_release() {
        let track = |id: i64, path: &str| Track {
            id,
            path: path.into(),
            state: crate::TrackState::Ok,
            error: None,
            meta: Metadata::default(),
        };
        let (a, b, other) = (
            track(1, "/x/1.flac"),
            track(2, "/x/2.flac"),
            track(3, "/y/3.flac"),
        );
        let tracks = HashMap::from([(1, &a), (2, &b), (3, &other)]);
        let result = |id, c| Identified {
            track_id: id,
            candidates: c,
            error: None,
        };
        let mut results = vec![
            result(1, vec![candidate("single", 100), candidate("album", 90)]),
            result(2, vec![candidate("album", 90), candidate("best-of", 80)]),
            result(3, vec![candidate("single", 100), candidate("album", 90)]),
        ];

        prefer_shared_release(&mut results, &tracks);

        assert_eq!(results[0].candidates[0].release_mbid, "album");
        assert_eq!(results[1].candidates[0].release_mbid, "album");
        assert_eq!(
            results[2].candidates[0].release_mbid, "single",
            "other folder untouched"
        );
    }

    #[test]
    fn cover_art_is_embedded_and_undone() {
        // A PNG signature is all the format check needs.
        const PNG: &[u8] = b"\x89PNG\r\n\x1a\n0000000000000000000000000";
        let (dir, mut lib, ids) = library_with(&[("long.flac", "a.flac")]);
        let path = dir.path().join("a.flac");
        let original = fs::read(&path).unwrap();
        let track_id = *ids.first().unwrap();
        let change = TagChange {
            field: Field::Genre,
            value: Some("Rock".into()),
        };
        let edit = TrackEdit {
            track_id,
            changes: vec![change],
            cover: Some(PNG.to_vec()),
        };

        let (change_set, report) = lib.apply_edits("cover", vec![edit]).unwrap();

        assert!(report.failed.is_empty(), "{:?}", report.failed);
        let file = lofty::read_from_path(&path).unwrap();
        use lofty::prelude::*;
        assert_eq!(file.primary_tag().unwrap().pictures().len(), 1);
        lib.undo(change_set).unwrap();
        assert!(fs::read(&path).unwrap() == original);
    }

    /// Calls the real MusicBrainz. Run by hand: cargo test -- --ignored
    #[test]
    #[ignore]
    fn live_musicbrainz_text_search() {
        let (_dir, mut lib, ids) = library_with(&[("long.flac", "a.flac")]);
        let change = |field, value: &str| TagChange {
            field,
            value: Some(value.into()),
        };
        let changes = [
            change(Field::Title, "Karma Police"),
            change(Field::Artist, "Radiohead"),
        ];
        lib.edit_tags(&ids.iter().copied().collect::<Vec<_>>(), &changes)
            .unwrap();
        let mut lookup = Lookup::new(Endpoints::default(), None).unwrap();

        let results = lib.identify(&mut lookup, &ids, |_, _| true).unwrap();

        assert_eq!(results[0].error, None);
        let first = &results[0].candidates[0];
        assert_eq!(first.title, "Karma Police");
        assert!(first.artist.contains("Radiohead"));
        assert!(first.track_no.is_some(), "{first:?}");
        println!("{:#?}", results[0].candidates);
    }

    #[test]
    fn search_text_is_escaped() {
        let meta = Metadata {
            title: Some(r#"Say "Hi" \ Now"#.into()),
            ..Default::default()
        };
        assert_eq!(
            search_query(&meta).unwrap(),
            r#"recording:"Say \"Hi\" \\ Now""#
        );
        assert_eq!(search_query(&Metadata::default()), None);
    }
}
