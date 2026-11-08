# Cantus

A desktop music library manager. Scan your folders, identify tracks by audio fingerprint, fix tags and file names with a preview and full undo, and share playlists as plain files.

Status: early development, no release yet.

## Features

- Scan folders into a SQLite library; unchanged files are skipped and moved files are recognized
- Edit tags on many files at once, with every change undoable from History
- Identify tracks by audio fingerprint (AcoustID) or by their tags (MusicBrainz), and review every change before it is written
- Organize files with an editable path template, previewed first; files are never overwritten
- Playlists as `.m3u8` and JSON; JSON files keep MusicBrainz ids, so they work across different folder layouts
- Torrent downloads with file selection; finished downloads wait in an Inbox and import without interrupting seeding
- Import audio from web links with yt-dlp, and sync a folder to cloud storage with rclone after a dry run

Tag writes and moves are journaled, so an interrupted change is settled on the next start and undo restores files byte for byte.

Identify is off until enabled in Settings. Matching by audio needs a free [AcoustID](https://acoustid.org) API key; without one, Cantus searches by tags.

Web links need `yt-dlp` and `ffmpeg`, and sync needs `rclone`, installed on your PATH. Torrents are off until enabled in Settings.

## Platforms

Windows, Linux and macOS. An Android build runs from source: it covers everything except web import and sync, scans the shared Music folder, and needs "All files access" granted in Android settings. Playlists can't be imported or exported there yet.

## Build

Requires stable Rust.

```
cargo run --release
```

On Linux, install the audio and X11/Wayland development headers first, e.g. on Debian/Ubuntu:

```
sudo apt install libasound2-dev pkg-config libxkbcommon-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev
```

For Android, install the NDK and `cargo-ndk`, then run `packaging/android/build-apk.sh cantus.apk`.

## License

Copyright (C) 2026 Aallyn. Licensed under GPL-3.0-only, see [LICENSE](LICENSE).
