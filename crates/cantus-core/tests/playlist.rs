use std::fs;
use std::path::Path;

use cantus_core::{Entry, Field, Library, TagChange, Template};

const FORMATS: &[&str] = &["flac", "mp3", "ogg"];

fn library_at(root: &Path, folder: &str) -> Library {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    fs::create_dir_all(root.join(folder)).unwrap();
    for ext in FORMATS {
        let dest = root.join(folder).join(format!("tone.{ext}"));
        fs::copy(fixtures.join(format!("tone.{ext}")), dest).unwrap();
    }
    let mut lib = Library::open_in_memory().unwrap();
    lib.scan(root).unwrap();
    lib
}

fn ids(lib: &Library) -> Vec<i64> {
    lib.tracks().unwrap().iter().map(|t| t.id).collect()
}

/// Gives every track a recording id made from its file extension.
fn tag_mbids(lib: &mut Library) {
    for track in lib.tracks().unwrap() {
        let ext = track
            .path
            .extension()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let change = TagChange {
            field: Field::RecordingMbid,
            value: Some(format!("mbid-{ext}")),
        };
        let (_, report) = lib.edit_tags(&[track.id], &[change]).unwrap();
        assert!(report.failed.is_empty(), "{:?}", report.failed);
    }
}

#[test]
fn json_playlist_moves_to_a_library_with_a_different_layout() {
    let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut source = library_at(a.path(), "old/layout");
    let mut target = library_at(b.path(), "completely/different/place");
    tag_mbids(&mut source);
    tag_mbids(&mut target);
    let playlist = source.create_playlist("Road Trip").unwrap();
    let mut order = ids(&source);
    order.reverse();
    source.add_to_playlist(playlist, &order).unwrap();
    let file = a.path().join("road.json");
    source.export_playlist(playlist, &file).unwrap();

    let report = target.import_playlist(&file).unwrap();

    assert_eq!((report.resolved, report.unresolved), (3, 0));
    assert_eq!(target.playlists().unwrap()[0].name, "Road Trip");
    let exts = |lib: &Library, id| -> Vec<String> {
        let entries = lib.playlist_entries(id).unwrap();
        entries
            .iter()
            .map(|e| e.recording_mbid.clone().unwrap())
            .collect()
    };
    assert_eq!(
        exts(&target, report.playlist_id),
        exts(&source, playlist),
        "order is kept"
    );
    assert!(
        target
            .playlist_entries(report.playlist_id)
            .unwrap()
            .iter()
            .all(|e| e
                .path
                .as_deref()
                .unwrap()
                .starts_with(b.path().to_str().unwrap()))
    );
}

#[test]
fn shared_files_contain_no_absolute_paths() {
    let dir = tempfile::tempdir().unwrap();
    let mut lib = library_at(dir.path(), "music");
    let playlist = lib.create_playlist("Mine").unwrap();
    lib.add_to_playlist(playlist, &ids(&lib)).unwrap();

    for name in ["out.json", "out.m3u8"] {
        let file = dir.path().join(name);
        lib.export_playlist(playlist, &file).unwrap();
        let text = fs::read_to_string(file).unwrap();
        assert!(text.contains("music/tone.flac"), "{text}");
        let leaked = dir.path().to_str().unwrap().replace('\\', "\\\\");
        assert!(
            !text.contains(&leaked) && !text.contains(dir.path().to_str().unwrap()),
            "{text}"
        );
    }
}

#[test]
fn m3u8_resolves_relative_to_the_playlist_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut lib = library_at(dir.path(), "music");
    let playlist = lib.create_playlist("Mine").unwrap();
    lib.add_to_playlist(playlist, &ids(&lib)).unwrap();
    fs::create_dir(dir.path().join("lists")).unwrap();
    let file = dir.path().join("lists/mine.m3u8");
    lib.export_playlist(playlist, &file).unwrap();
    assert!(
        fs::read_to_string(&file)
            .unwrap()
            .contains("../music/tone.mp3")
    );

    let report = lib.import_playlist(&file).unwrap();

    assert_eq!((report.resolved, report.unresolved), (3, 0));
    assert_eq!(lib.playlists().unwrap().len(), 2);
}

#[test]
fn unmatched_entries_are_kept_and_listed() {
    let dir = tempfile::tempdir().unwrap();
    let mut lib = library_at(dir.path(), "music");
    let file = dir.path().join("list.m3u8");
    fs::write(
        &file,
        "#EXTM3U\n#EXTINF:10,Nobody - Nothing\ngone/missing.mp3\nmusic/tone.flac\n",
    )
    .unwrap();

    let report = lib.import_playlist(&file).unwrap();

    assert_eq!((report.resolved, report.unresolved), (1, 1));
    let entries = lib.playlist_entries(report.playlist_id).unwrap();
    assert_eq!(entries[0].track_id, None);
    assert_eq!(entries[0].title.as_deref(), Some("Nothing"));
    assert_eq!(entries[0].path.as_deref(), Some("gone/missing.mp3"));
    assert!(entries[1].track_id.is_some());
}

#[test]
fn entries_fall_back_to_artist_title_and_duration() {
    let dir = tempfile::tempdir().unwrap();
    let mut lib = library_at(dir.path(), "music");
    let file = dir.path().join("list.m3u8");
    let text = "#EXTINF:0,Test Artist - test tone\nelsewhere/a.mp3\n\
                #EXTINF:300,Test Artist - Test Tone\nelsewhere/too-long.mp3\n";
    fs::write(&file, text).unwrap();

    let report = lib.import_playlist(&file).unwrap();

    assert_eq!((report.resolved, report.unresolved), (1, 1));
}

#[test]
fn bad_files_are_rejected_without_creating_a_playlist() {
    let dir = tempfile::tempdir().unwrap();
    let mut lib = library_at(dir.path(), "music");
    let wrong = dir.path().join("wrong.json");
    fs::write(&wrong, r#"{"format":"other/9","name":"x","entries":[]}"#).unwrap();
    let garbage = dir.path().join("garbage.json");
    fs::write(&garbage, "not json").unwrap();

    assert!(lib.import_playlist(&wrong).is_err());
    assert!(lib.import_playlist(&garbage).is_err());
    assert!(lib.playlists().unwrap().is_empty());
}

#[test]
fn playlists_follow_tracks_through_organize() {
    let dir = tempfile::tempdir().unwrap();
    let mut lib = library_at(dir.path(), "music");
    let playlist = lib.create_playlist("Mine").unwrap();
    lib.add_to_playlist(playlist, &ids(&lib)).unwrap();

    let plan = lib
        .plan_organize(&Template::parse("sorted/{title}.{ext}").unwrap(), None)
        .unwrap();
    lib.organize(plan).unwrap();

    for entry in lib.playlist_entries(playlist).unwrap() {
        assert!(Path::new(&entry.path.unwrap()).starts_with(dir.path().join("sorted")));
    }
}

#[test]
fn reordering_removing_and_deleting() {
    let dir = tempfile::tempdir().unwrap();
    let mut lib = library_at(dir.path(), "music");
    let playlist = lib.create_playlist("Mine").unwrap();
    lib.add_to_playlist(playlist, &ids(&lib)).unwrap();

    let mut entries: Vec<Entry> = lib.playlist_entries(playlist).unwrap();
    entries.swap(0, 2);
    entries.remove(1);
    let expected: Vec<_> = entries.iter().map(|e| e.track_id).collect();
    lib.set_playlist_entries(playlist, &entries).unwrap();
    let saved = lib.playlist_entries(playlist).unwrap();
    assert_eq!(
        saved.iter().map(|e| e.track_id).collect::<Vec<_>>(),
        expected
    );

    lib.delete_playlist(playlist).unwrap();
    assert!(lib.playlists().unwrap().is_empty());
}
