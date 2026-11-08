//! A real download between two engines on the loopback interface. No internet involved.

use std::fs;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use cantus_core::{
    Field, Handoff, Library, TagChange, Template, TorrentConfig, TorrentSource, TorrentState,
    Torrents,
};
use librqbit::{CreateTorrentOptions, ListenerMode, ListenerOptions, Session, SessionOptions};

const AUDIO: &[&str] = &["tone.flac", "tone.mp3"];

struct Seeder {
    _session: Arc<Session>,
    _runtime: tokio::runtime::Runtime,
    addr: SocketAddr,
    torrent: Vec<u8>,
}

fn loopback() -> SocketAddr {
    (Ipv4Addr::LOCALHOST, 0).into()
}

fn seed(folder: &Path) -> Seeder {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (session, torrent) = runtime.block_on(async {
        let options = SessionOptions {
            dht: None,
            disable_trackers: true,
            disable_local_service_discovery: true,
            persistence: None,
            listen: Some(ListenerOptions {
                mode: ListenerMode::TcpOnly,
                listen_addr: loopback(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let session = Session::new_with_opts(folder.to_path_buf(), options)
            .await
            .unwrap();
        let (created, handle) = session
            .create_and_serve_torrent(folder, CreateTorrentOptions::default())
            .await
            .unwrap();
        handle.wait_until_initialized().await.unwrap();
        (session, created.as_bytes().unwrap().to_vec())
    });
    let addr = session.listen_addr().unwrap();
    Seeder {
        _session: session,
        _runtime: runtime,
        addr,
        torrent,
    }
}

fn engine(root: &Path, peer: SocketAddr) -> Torrents {
    Torrents::start(TorrentConfig {
        download_dir: root.join("downloads"),
        state_dir: root.join("state"),
        listen: loopback(),
        dht: false,
        proxy: None,
        download_limit: None,
        upload_limit: None,
        peers: vec![peer],
    })
    .unwrap()
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "timed out: {what}"
        );
        sleep(Duration::from_millis(50));
    }
}

/// Seeds fixtures plus a non-audio file and downloads them into `root/downloads`.
fn downloaded(root: &Path) -> (Seeder, Torrents, Vec<PathBuf>) {
    let source = root.join("seed/album");
    fs::create_dir_all(&source).unwrap();
    let fixtures = cantus_core::test_fixtures();
    for name in AUDIO {
        fs::copy(fixtures.join(name), source.join(name)).unwrap();
    }
    fs::write(source.join("info.nfo"), b"not audio").unwrap();
    let seeder = seed(&source);

    let torrents = engine(root, seeder.addr);
    let id = torrents
        .add(TorrentSource::File(seeder.torrent.clone()))
        .unwrap();
    let files = torrents.files(id);
    assert_eq!(files.len(), 3, "{files:?}");
    assert!(
        torrents.list()[0].state == TorrentState::Paused,
        "added paused"
    );
    torrents.resume(id).unwrap();
    wait_until("download", || torrents.list()[0].finished);

    let locations = files.into_iter().map(|f| f.location).collect();
    (seeder, torrents, locations)
}

fn library_with_root(root: &Path) -> Library {
    fs::create_dir_all(root).unwrap();
    let mut lib = Library::open_in_memory().unwrap();
    lib.scan(root).unwrap();
    lib
}

#[test]
fn finished_download_flows_through_inbox_into_the_library_and_back() {
    let dir = tempfile::tempdir().unwrap();
    let (_seeder, torrents, files) = downloaded(dir.path());
    let snapshot = |paths: &[PathBuf]| -> Vec<Vec<u8>> {
        paths.iter().map(|p| fs::read(p).unwrap()).collect()
    };
    let seeded = snapshot(&files);

    let music = dir.path().join("music");
    let mut lib = library_with_root(&music);
    assert_eq!(lib.collect_finished(&torrents).unwrap().len(), 1);
    assert_eq!(
        lib.collect_finished(&torrents).unwrap().len(),
        0,
        "only once"
    );
    let batch = lib.inbox().unwrap().remove(0);
    assert_eq!(batch.files.len(), 2, "audio only: {:?}", batch.files);
    assert_eq!(batch.origin, "torrent");

    let template = Template::parse("{album}/{title}.{ext}").unwrap();
    let plan = lib.plan_import(batch.id, &music, &template).unwrap();
    let (change_set, report) = lib.import_inbox(batch.id, plan, Handoff::Copy).unwrap();

    assert_eq!(
        (report.done, report.failed.len()),
        (2, 0),
        "{:?}",
        report.failed
    );
    assert!(music.join("Test Album/Test Tone.flac").is_file());
    let tracks = lib.tracks().unwrap();
    assert_eq!(tracks.len(), 2);
    assert_eq!(tracks[0].meta.artist.as_deref(), Some("Test Artist"));
    assert!(lib.inbox().unwrap()[0].imported_by.is_some());

    // Retagging the library copy must leave what the torrent seeds alone.
    let ids: Vec<i64> = tracks.iter().map(|t| t.id).collect();
    let change = TagChange {
        field: Field::Title,
        value: Some("Retagged".into()),
    };
    let (edit, _) = lib.edit_tags(&ids, &[change]).unwrap();
    assert!(snapshot(&files) == seeded, "seeded files are untouched");
    assert_eq!(torrents.list()[0].state, TorrentState::Seeding);

    let report = lib.undo(change_set).unwrap();
    assert_eq!(
        report.failed.len(),
        2,
        "edited copies are protected: {:?}",
        report.failed
    );
    lib.undo(edit).unwrap();
    let report = lib.undo(change_set).unwrap();
    assert_eq!(report.done, 2, "{:?}", report.failed);
    assert!(lib.tracks().unwrap().is_empty());
    assert!(!music.join("Test Album").exists());
    assert_eq!(
        lib.inbox().unwrap()[0].imported_by,
        None,
        "back in the Inbox"
    );
    assert!(snapshot(&files) == seeded);
}

#[test]
fn hardlinked_import_can_be_retagged_without_touching_the_download() {
    let dir = tempfile::tempdir().unwrap();
    let (_seeder, torrents, files) = downloaded(dir.path());
    let original: Vec<_> = files.iter().map(|p| fs::read(p).unwrap()).collect();
    let music = dir.path().join("music");
    let mut lib = library_with_root(&music);
    lib.collect_finished(&torrents).unwrap();
    let batch = lib.inbox().unwrap().remove(0);
    let template = Template::parse("{title}.{ext}").unwrap();
    let plan = lib.plan_import(batch.id, &music, &template).unwrap();

    let (_, report) = lib.import_inbox(batch.id, plan, Handoff::Hardlink).unwrap();
    assert_eq!(report.failed.len(), 0, "{:?}", report.failed);

    let ids: Vec<i64> = lib.tracks().unwrap().iter().map(|t| t.id).collect();
    let change = TagChange {
        field: Field::Title,
        value: Some("Retagged".into()),
    };
    lib.edit_tags(&ids, &[change]).unwrap();

    assert!(
        lib.tracks()
            .unwrap()
            .iter()
            .all(|t| t.meta.title.as_deref() == Some("Retagged"))
    );
    let after: Vec<_> = files.iter().map(|p| fs::read(p).unwrap()).collect();
    assert!(after == original);
}

#[test]
fn file_selection_limits_the_download_and_the_inbox() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("seed/album");
    fs::create_dir_all(&source).unwrap();
    let fixtures = cantus_core::test_fixtures();
    for name in AUDIO {
        fs::copy(fixtures.join(name), source.join(name)).unwrap();
    }
    let seeder = seed(&source);
    let torrents = engine(dir.path(), seeder.addr);
    let id = torrents
        .add(TorrentSource::File(seeder.torrent.clone()))
        .unwrap();
    let flac = torrents
        .files(id)
        .iter()
        .find(|f| f.path.ends_with("tone.flac"))
        .unwrap()
        .index;

    torrents
        .select_files(id, &std::collections::BTreeSet::from([flac]))
        .unwrap();
    torrents.resume(id).unwrap();
    wait_until("download", || torrents.list()[0].finished);

    let mut lib = Library::open_in_memory().unwrap();
    lib.collect_finished(&torrents).unwrap();
    let batch = lib.inbox().unwrap().remove(0);
    assert_eq!(batch.files.len(), 1);
    assert!(batch.files[0].ends_with("tone.flac"));
}

#[test]
fn torrents_survive_a_restart_and_can_be_removed() {
    let dir = tempfile::tempdir().unwrap();
    let (seeder, torrents, files) = downloaded(dir.path());
    drop(torrents);

    let torrents = engine(dir.path(), seeder.addr);
    wait_until("resume", || {
        torrents.list().first().is_some_and(|t| t.finished)
    });
    let id = torrents.list()[0].id;

    torrents.remove(id, false).unwrap();
    assert!(torrents.list().is_empty());
    assert!(files[0].is_file(), "removing keeps the files");
}

#[test]
fn only_magnet_links_are_accepted_as_links() {
    let dir = tempfile::tempdir().unwrap();
    let torrents = engine(dir.path(), loopback());
    let source = TorrentSource::Magnet("http://example.com/x.torrent".into());
    assert!(torrents.add(source).is_err());
    assert!(
        torrents
            .add(TorrentSource::File(b"garbage".to_vec()))
            .is_err()
    );
}
