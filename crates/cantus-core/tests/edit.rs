use std::fs;
use std::path::{Path, PathBuf};

use cantus_core::{Error, Field, Library, TagChange};

const FORMATS: &[&str] = &["aiff", "flac", "m4a", "mp3", "ogg", "opus", "wav", "wv"];

/// `copies` copies of every fixture in a temp folder, scanned into a library.
fn library(copies: usize) -> (tempfile::TempDir, Library) {
    let dir = tempfile::tempdir().unwrap();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    for i in 0..copies {
        for ext in FORMATS {
            let dest = dir.path().join(format!("{i:03}.{ext}"));
            fs::copy(fixtures.join(format!("tone.{ext}")), dest).unwrap();
        }
    }
    let mut lib = Library::open_in_memory().unwrap();
    lib.scan(dir.path()).unwrap();
    (dir, lib)
}

fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let path = e.unwrap().path();
            let bytes = fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect();
    files.sort();
    files
}

fn set(field: Field, value: &str) -> TagChange {
    TagChange {
        field,
        value: Some(value.into()),
    }
}

fn ids(lib: &Library) -> Vec<i64> {
    lib.tracks().unwrap().iter().map(|t| t.id).collect()
}

#[test]
fn editing_and_undoing_100_files_restores_every_byte() {
    let (dir, mut lib) = library(13);
    let before = snapshot(dir.path());
    let changes = [
        set(Field::Title, "New Title"),
        set(Field::Artist, "Ünïcödé Artist"),
        set(Field::Year, "1999"),
        TagChange {
            field: Field::Genre,
            value: None,
        },
    ];

    let (change_set, report) = lib.edit_tags(&ids(&lib), &changes).unwrap();

    assert_eq!(
        (report.done, report.failed.len()),
        (104, 0),
        "{:?}",
        report.failed
    );
    for t in lib.tracks().unwrap() {
        let name = t.path.display();
        assert_eq!(t.meta.title.as_deref(), Some("New Title"), "{name}");
        assert_eq!(t.meta.artist.as_deref(), Some("Ünïcödé Artist"), "{name}");
        assert_eq!(t.meta.year, Some(1999), "{name}");
        assert_eq!(t.meta.genre, None, "{name}");
        assert_eq!(
            t.meta.album.as_deref(),
            Some("Test Album"),
            "{name}: untouched"
        );
    }

    let report = lib.undo(change_set).unwrap();

    assert_eq!(
        (report.done, report.failed.len()),
        (104, 0),
        "{:?}",
        report.failed
    );
    assert!(
        snapshot(dir.path()) == before,
        "every file is byte-identical again"
    );
    assert!(
        lib.tracks()
            .unwrap()
            .iter()
            .all(|t| t.meta.title.as_deref() == Some("Test Tone"))
    );
    assert_eq!(lib.history().unwrap()[0].undone, 104);
}

#[test]
fn invalid_numbers_are_rejected_before_any_file_is_touched() {
    let (dir, mut lib) = library(1);
    let before = snapshot(dir.path());

    let err = lib
        .edit_tags(&ids(&lib), &[set(Field::TrackNo, "three")])
        .unwrap_err();

    assert!(matches!(err, Error::InvalidInput(_)));
    assert!(snapshot(dir.path()) == before);
    assert!(lib.history().unwrap().is_empty());
}

#[test]
fn a_missing_file_fails_alone() {
    let (dir, mut lib) = library(1);
    fs::remove_file(dir.path().join("000.mp3")).unwrap();

    let (_, report) = lib
        .edit_tags(&ids(&lib), &[set(Field::Title, "Another Title")])
        .unwrap();

    assert_eq!(report.done, FORMATS.len() - 1);
    assert_eq!(report.failed.len(), 1);
    assert!(report.failed[0].0.ends_with("000.mp3"));
}

/// lofty 0.25.5 panics when an MP4 tag shrinks by exactly its padding header (8 bytes).
/// The edit must then fail cleanly instead of taking the batch down.
#[test]
fn a_format_library_panic_fails_one_file_cleanly() {
    let (dir, mut lib) = library(1);
    let m4a = dir.path().join("000.m4a");
    let original = fs::read(&m4a).unwrap();

    let (_, report) = lib
        .edit_tags(&ids(&lib), &[set(Field::Title, "X")])
        .unwrap();

    assert!(report.done >= FORMATS.len() - 1, "{:?}", report.failed);
    if let Some((path, _)) = report.failed.first() {
        assert_eq!(path, &m4a);
        assert_eq!(fs::read(&m4a).unwrap(), original);
    }
}

#[cfg(windows)]
#[test]
fn a_locked_file_is_skipped_and_left_intact() {
    use std::os::windows::fs::OpenOptionsExt;

    let (dir, mut lib) = library(1);
    let locked = dir.path().join("000.flac");
    let original = fs::read(&locked).unwrap();
    // Another program reading the file without allowing it to be replaced.
    const FILE_SHARE_READ: u32 = 1;
    let handle = fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(&locked)
        .unwrap();

    let (_, report) = lib
        .edit_tags(&ids(&lib), &[set(Field::Title, "Another Title")])
        .unwrap();

    drop(handle);
    assert_eq!(report.done, FORMATS.len() - 1);
    assert_eq!(report.failed.len(), 1, "{:?}", report.failed);
    assert_eq!(fs::read(&locked).unwrap(), original);
    let leftovers = fs::read_dir(dir.path()).unwrap().count();
    assert_eq!(leftovers, FORMATS.len(), "no temp file left behind");
}
