use std::fs;
use std::path::PathBuf;

use cantus_core::{Library, ScanReport, Track, TrackState};
use lofty::config::WriteOptions;
use lofty::prelude::*;

const FORMATS: &[&str] = &["aiff", "flac", "m4a", "mp3", "ogg", "opus", "wav", "wv"];

fn fixture(ext: &str) -> PathBuf {
    cantus_core::test_fixtures().join(format!("tone.{ext}"))
}

/// A temp folder holding one copy of every fixture, plus a scanned library.
fn scanned() -> (tempfile::TempDir, Library) {
    let dir = tempfile::tempdir().unwrap();
    for ext in FORMATS {
        fs::copy(fixture(ext), dir.path().join(format!("tone.{ext}"))).unwrap();
    }
    let mut lib = Library::open_in_memory().unwrap();
    lib.scan(dir.path()).unwrap();
    (dir, lib)
}

fn track(lib: &Library, name: &str) -> Track {
    lib.tracks()
        .unwrap()
        .into_iter()
        .find(|t| t.path.file_name().unwrap() == name)
        .unwrap()
}

#[test]
fn reads_tags_and_properties_of_every_format() {
    let (_dir, lib) = scanned();
    let tracks = lib.tracks().unwrap();
    assert_eq!(tracks.len(), FORMATS.len());

    for t in tracks {
        let m = &t.meta;
        let name = t.path.display();
        assert_eq!(t.state, TrackState::Ok, "{name}: {:?}", t.error);
        assert_eq!(m.title.as_deref(), Some("Test Tone"), "{name}");
        assert_eq!(m.artist.as_deref(), Some("Test Artist"), "{name}");
        assert_eq!(m.album.as_deref(), Some("Test Album"), "{name}");
        assert_eq!(m.track_no, Some(3), "{name}");
        if t.path.extension().unwrap() != "wav" {
            // RIFF INFO has no album artist field.
            assert_eq!(
                m.album_artist.as_deref(),
                Some("Test Album Artist"),
                "{name}"
            );
            assert_eq!(m.disc_no, Some(2), "{name}");
        }
        assert_eq!(m.year, Some(2024), "{name}");
        assert_eq!(m.channels, Some(1), "{name}");
        let ms = m.duration_ms.unwrap();
        assert!((450..=600).contains(&ms), "{name}: {ms} ms");
    }
}

#[test]
fn rescan_of_unchanged_folder_changes_nothing() {
    let (dir, mut lib) = scanned();
    let before = lib.tracks().unwrap();

    let report = lib.scan(dir.path()).unwrap();

    assert_eq!(
        report,
        ScanReport {
            unchanged: FORMATS.len(),
            ..Default::default()
        }
    );
    assert_eq!(lib.tracks().unwrap(), before);
}

#[test]
fn moved_file_keeps_its_id() {
    let (dir, mut lib) = scanned();
    let id = track(&lib, "tone.flac").id;
    fs::create_dir(dir.path().join("sub")).unwrap();
    fs::rename(
        dir.path().join("tone.flac"),
        dir.path().join("sub/moved.flac"),
    )
    .unwrap();

    let report = lib.scan(dir.path()).unwrap();

    assert_eq!(report.moved, 1);
    assert_eq!((report.added, report.missing), (0, 0));
    let moved = track(&lib, "moved.flac");
    assert_eq!(moved.id, id);
    assert_eq!(moved.meta.title.as_deref(), Some("Test Tone"));
}

#[test]
fn deleted_file_is_marked_missing_and_restored_when_back() {
    let (dir, mut lib) = scanned();
    let path = dir.path().join("tone.mp3");
    let backup = fs::read(&path).unwrap();
    fs::remove_file(&path).unwrap();

    assert_eq!(lib.scan(dir.path()).unwrap().missing, 1);
    assert_eq!(track(&lib, "tone.mp3").state, TrackState::Missing);

    fs::write(&path, backup).unwrap();
    let report = lib.scan(dir.path()).unwrap();
    assert_eq!(report.updated, 1);
    assert_eq!(track(&lib, "tone.mp3").state, TrackState::Ok);
}

#[test]
fn changed_file_is_reread() {
    let (dir, mut lib) = scanned();
    let path = dir.path().join("tone.flac");
    let mut file = lofty::read_from_path(&path).unwrap();
    file.primary_tag_mut().unwrap().set_title("Changed".into());
    file.save_to_path(&path, WriteOptions::default()).unwrap();

    let report = lib.scan(dir.path()).unwrap();

    assert_eq!(report.updated, 1);
    assert_eq!(
        track(&lib, "tone.flac").meta.title.as_deref(),
        Some("Changed")
    );
}

#[test]
fn corrupt_files_are_kept_as_unreadable_and_other_files_ignored() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("broken.mp3"), b"not audio").unwrap();
    fs::write(dir.path().join("notes.txt"), b"ignored").unwrap();
    let mut lib = Library::open_in_memory().unwrap();

    let report = lib.scan(dir.path()).unwrap();

    assert_eq!((report.added, report.unreadable), (1, 1));
    let t = track(&lib, "broken.mp3");
    assert_eq!(t.state, TrackState::Unreadable);
    assert!(t.error.is_some());
}

#[test]
fn roots_are_recorded_once() {
    let (dir, mut lib) = scanned();
    lib.scan(dir.path()).unwrap();
    assert_eq!(
        lib.roots().unwrap(),
        vec![std::path::absolute(dir.path()).unwrap()]
    );
}
