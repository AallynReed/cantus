use std::fs;
use std::path::{Path, PathBuf};

use cantus_core::{Field, Library, TagChange, Template};
use walkdir::WalkDir;

const FORMATS: &[&str] = &["aiff", "flac", "m4a", "mp3", "ogg", "opus", "wav", "wv"];

fn library(copies: usize, sub: &str) -> (tempfile::TempDir, Library) {
    let dir = tempfile::tempdir().unwrap();
    let fixtures = cantus_core::test_fixtures();
    fs::create_dir_all(dir.path().join(sub)).unwrap();
    for i in 0..copies {
        for ext in FORMATS {
            let dest = dir.path().join(sub).join(format!("{i:03}.{ext}"));
            fs::copy(fixtures.join(format!("tone.{ext}")), dest).unwrap();
        }
    }
    let mut lib = Library::open_in_memory().unwrap();
    lib.scan(dir.path()).unwrap();
    (dir, lib)
}

/// Every file and folder under `dir` with its bytes.
fn tree(dir: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
    let mut entries: Vec<_> = WalkDir::new(dir)
        .min_depth(1)
        .into_iter()
        .map(|e| {
            let e = e.unwrap();
            let bytes = e.file_type().is_file().then(|| fs::read(e.path()).unwrap());
            (e.into_path(), bytes)
        })
        .collect();
    entries.sort();
    entries
}

fn template(source: &str) -> Template {
    Template::parse(source).unwrap()
}

fn set(lib: &mut Library, field: Field, value: &str) {
    let ids: Vec<i64> = lib.tracks().unwrap().iter().map(|t| t.id).collect();
    let change = TagChange {
        field,
        value: Some(value.into()),
    };
    let (_, report) = lib.edit_tags(&ids, &[change]).unwrap();
    assert!(report.failed.is_empty(), "{:?}", report.failed);
}

#[test]
fn organizing_and_undoing_100_files_restores_the_tree() {
    let (dir, mut lib) = library(13, "incoming/deep");
    let before = tree(dir.path());
    let ids_before: Vec<i64> = lib.tracks().unwrap().iter().map(|t| t.id).collect();

    let plan = lib
        .plan_organize(&template("{artist}/{album}/{title}.{ext}"), None)
        .unwrap();
    assert_eq!(plan.len(), 104);
    assert_eq!(
        plan.iter().filter(|m| m.collision).count(),
        104 - FORMATS.len()
    );
    assert_eq!(tree(dir.path()), before, "planning touches nothing");

    let (change_set, report) = lib.organize(plan).unwrap();

    assert_eq!(
        (report.done, report.failed.len()),
        (104, 0),
        "{:?}",
        report.failed
    );
    let album = dir.path().join("Test Artist/Test Album");
    assert!(album.join("Test Tone.flac").is_file());
    assert!(album.join("Test Tone (2).flac").is_file());
    assert!(
        !dir.path().join("incoming").exists(),
        "emptied folders are removed"
    );
    let tracks = lib.tracks().unwrap();
    assert!(
        tracks
            .iter()
            .all(|t| t.path.starts_with(&album) && t.path.is_file())
    );
    assert_eq!(tracks.iter().map(|t| t.id).collect::<Vec<_>>(), ids_before);

    let report = lib.undo(change_set).unwrap();

    assert_eq!(
        (report.done, report.failed.len()),
        (104, 0),
        "{:?}",
        report.failed
    );
    assert!(tree(dir.path()) == before, "every path and byte is back");
    assert!(lib.tracks().unwrap().iter().all(|t| t.path.is_file()));
}

#[test]
fn organizing_twice_changes_nothing() {
    let (_dir, mut lib) = library(2, "a");
    let t = template("{album}/{title}.{ext}");
    let plan = lib.plan_organize(&t, None).unwrap();
    assert_eq!(lib.organize(plan).unwrap().1.failed.len(), 0);
    assert_eq!(lib.plan_organize(&t, None).unwrap(), []);
}

#[test]
fn rescan_after_organize_sees_no_changes() {
    let (dir, mut lib) = library(1, "a");
    let plan = lib
        .plan_organize(&template("{album}/{title}.{ext}"), None)
        .unwrap();
    lib.organize(plan).unwrap();
    let report = lib.scan(dir.path()).unwrap();
    assert_eq!(
        (report.unchanged, report.added, report.missing),
        (FORMATS.len(), 0, 0)
    );
}

#[test]
fn a_file_appearing_at_the_target_is_never_overwritten() {
    let (dir, mut lib) = library(1, "a");
    let plan = lib
        .plan_organize(&template("b/{title}.{ext}"), None)
        .unwrap();
    fs::create_dir_all(dir.path().join("b")).unwrap();
    let intruder = dir.path().join("b/Test Tone.mp3");
    fs::write(&intruder, b"precious").unwrap();

    let (_, report) = lib.organize(plan).unwrap();

    assert_eq!(fs::read(&intruder).unwrap(), b"precious");
    assert_eq!((report.done, report.failed.len()), (FORMATS.len() - 1, 1));
    assert!(
        report.failed[0].0.ends_with("a/000.mp3"),
        "{:?}",
        report.failed
    );
    assert!(dir.path().join("a/000.mp3").is_file());
}

#[test]
fn hostile_tags_stay_inside_the_root() {
    let (dir, mut lib) = library(1, "a");
    set(&mut lib, Field::Artist, "../../../etc");
    set(&mut lib, Field::Album, "CON");
    set(&mut lib, Field::Title, "what?: *a/b\\c. ");

    let plan = lib
        .plan_organize(&template("{artist}/{album}/{title}.{ext}"), None)
        .unwrap();

    for m in &plan {
        assert!(m.to.starts_with(dir.path()), "{:?}", m.to);
        assert_eq!(
            m.to.strip_prefix(dir.path()).unwrap().components().count(),
            3
        );
    }
    let (_, report) = lib.organize(plan).unwrap();
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert!(dir.path().join(".._.._.._etc/_CON").is_dir());
}

#[test]
fn case_only_rename_works() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = cantus_core::test_fixtures().join("tone.flac");
    fs::copy(fixture, dir.path().join("test tone.flac")).unwrap();
    let mut lib = Library::open_in_memory().unwrap();
    lib.scan(dir.path()).unwrap();

    let plan = lib.plan_organize(&template("{title}.{ext}"), None).unwrap();
    assert!(!plan[0].collision);
    let (change_set, report) = lib.organize(plan).unwrap();

    assert_eq!(report.done, 1, "{:?}", report.failed);
    let name = || {
        fs::read_dir(dir.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .file_name()
    };
    assert_eq!(name(), "Test Tone.flac");
    lib.undo(change_set).unwrap();
    assert_eq!(name(), "test tone.flac");
}

#[test]
fn paths_longer_than_windows_max_path_work() {
    let (dir, mut lib) = library(1, "a");
    set(&mut lib, Field::Album, &"album ".repeat(30));
    set(&mut lib, Field::Artist, &"artist ".repeat(25));

    let plan = lib
        .plan_organize(&template("{artist}/{album}/{title}.{ext}"), None)
        .unwrap();
    assert!(plan.iter().all(|m| m.to.as_os_str().len() > 260));
    let before = tree(dir.path());
    let (change_set, report) = lib.organize(plan).unwrap();

    assert_eq!(report.failed.len(), 0, "{:?}", report.failed);
    assert_eq!(lib.undo(change_set).unwrap().failed.len(), 0);
    assert!(tree(dir.path()) == before);
}

#[test]
fn undoing_an_edit_finds_the_file_after_it_was_organized() {
    let (dir, mut lib) = library(1, "a");
    let ids: Vec<i64> = lib.tracks().unwrap().iter().map(|t| t.id).collect();
    let change = TagChange {
        field: Field::Genre,
        value: Some("Rock".into()),
    };
    let (edit, _) = lib.edit_tags(&ids, &[change]).unwrap();
    let plan = lib
        .plan_organize(&template("b/{title}.{ext}"), None)
        .unwrap();
    lib.organize(plan).unwrap();

    let report = lib.undo(edit).unwrap();

    assert_eq!(
        (report.done, report.failed.len()),
        (FORMATS.len(), 0),
        "{:?}",
        report.failed
    );
    assert!(
        lib.tracks()
            .unwrap()
            .iter()
            .all(|t| t.meta.genre.as_deref() == Some("Test"))
    );
    assert!(dir.path().join("b/Test Tone.flac").is_file());
}
