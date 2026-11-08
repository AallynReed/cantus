//! Tests the external-tool code against this test program acting as a fake tool, so they run
//! the same on every OS without yt-dlp or rclone installed.

use std::fs;
use std::path::{Path, PathBuf};

use cantus_core::{download_audio, preview_sync, run_sync, tool_version};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version") => println!("fake-tool 1.2.3"),
        Some("--ignore-config") => fake_yt_dlp(&args),
        Some("copy" | "sync") => fake_rclone(&args),
        _ => {
            let tests: [(&str, fn()); 8] = [
                ("detects_versions", detects_versions),
                (
                    "download_returns_audio_in_the_folder",
                    download_returns_audio_in_the_folder,
                ),
                (
                    "download_failure_reports_the_tool_error",
                    download_failure_reports_the_tool_error,
                ),
                ("missing_tool_is_an_error", missing_tool_is_an_error),
                (
                    "only_web_links_are_downloaded",
                    only_web_links_are_downloaded,
                ),
                (
                    "files_outside_the_folder_are_ignored",
                    files_outside_the_folder_are_ignored,
                ),
                ("sync_previews_before_it_runs", sync_previews_before_it_runs),
                ("bad_remotes_are_rejected", bad_remotes_are_rejected),
            ];
            for (name, test) in tests {
                test();
                println!("test {name} ... ok");
            }
        }
    }
}

fn value_after<'a>(args: &'a [String], flag: &str) -> &'a str {
    let at = args.iter().position(|a| a == flag).unwrap();
    &args[at + 1]
}

fn fake_yt_dlp(args: &[String]) {
    let dest = PathBuf::from(value_after(args, "--paths"));
    let url = value_after(args, "--");
    if url.contains("fail") {
        eprintln!("ERROR: this video is unavailable");
        std::process::exit(1);
    }
    if url.contains("outside") {
        println!("{}", dest.parent().unwrap().join("evil.mp3").display());
        return;
    }
    let file = dest.join("song.mp3");
    fs::write(&file, b"audio").unwrap();
    fs::write(dest.join("thumbnail.jpg"), b"x").unwrap();
    println!("{}", file.display());
    println!("{}", dest.join("thumbnail.jpg").display());
}

fn fake_rclone(args: &[String]) {
    let at = args.iter().position(|a| a == "--").unwrap();
    let source = Path::new(&args[at + 1]);
    let log = source.join("calls.txt");
    let mut calls = fs::read_to_string(&log).unwrap_or_default();
    calls += &format!("{}\n", args.join(" "));
    fs::write(&log, calls).unwrap();
    if args.iter().any(|a| a == "--dry-run") {
        eprintln!("NOTICE: a.flac: Skipped copy as --dry-run is set (size 1)");
        eprintln!("NOTICE: b.flac: Skipped copy as --dry-run is set (size 1)");
        if args[0] == "sync" {
            eprintln!("NOTICE: old.flac: Skipped delete as --dry-run is set (size 1)");
        }
    }
}

fn fake() -> PathBuf {
    std::env::current_exe().unwrap()
}

fn detects_versions() {
    assert_eq!(
        tool_version(&fake(), "--version").as_deref(),
        Some("fake-tool 1.2.3")
    );
    assert_eq!(
        tool_version(Path::new("no-such-tool-here"), "--version"),
        None
    );
}

fn download_returns_audio_in_the_folder() {
    let dir = tempfile::tempdir().unwrap();
    let files = download_audio(&fake(), "https://example.com/watch?v=1&x=2", dir.path()).unwrap();
    assert_eq!(files, [dir.path().join("song.mp3")]);
}

fn download_failure_reports_the_tool_error() {
    let dir = tempfile::tempdir().unwrap();
    let err = download_audio(&fake(), "https://example.com/fail", dir.path()).unwrap_err();
    assert!(err.contains("unavailable"), "{err}");
}

fn missing_tool_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let err = download_audio(
        Path::new("no-such-tool-here"),
        "https://e.com/a",
        dir.path(),
    )
    .unwrap_err();
    assert!(err.contains("cannot run"), "{err}");
}

fn only_web_links_are_downloaded() {
    let dir = tempfile::tempdir().unwrap();
    for url in ["--exec=evil", "file:///etc/passwd", "ftp://x/y"] {
        assert!(download_audio(&fake(), url, dir.path()).is_err(), "{url}");
    }
}

fn files_outside_the_folder_are_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let inner = dir.path().join("inner");
    fs::create_dir(&inner).unwrap();
    fs::write(dir.path().join("evil.mp3"), b"x").unwrap();
    assert!(download_audio(&fake(), "https://e.com/outside", &inner).is_err());
}

fn sync_previews_before_it_runs() {
    let dir = tempfile::tempdir().unwrap();
    let plan = preview_sync(&fake(), dir.path(), "remote:music", true).unwrap();
    assert_eq!((plan.copies, plan.deletes), (2, 1));
    let calls = fs::read_to_string(dir.path().join("calls.txt")).unwrap();
    assert_eq!(calls.lines().count(), 1);
    assert!(calls.contains("sync --dry-run --"), "{calls}");

    run_sync(&fake(), &plan).unwrap();
    let calls = fs::read_to_string(dir.path().join("calls.txt")).unwrap();
    assert!(
        calls.lines().nth(1).unwrap().starts_with("sync -- "),
        "{calls}"
    );

    let plan = preview_sync(&fake(), dir.path(), "remote:music", false).unwrap();
    assert_eq!(plan.deletes, 0, "plain copy never deletes");
}

fn bad_remotes_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    for remote in ["--config=/evil:x", "no-colon"] {
        assert!(
            preview_sync(&fake(), dir.path(), remote, false).is_err(),
            "{remote}"
        );
    }
}
