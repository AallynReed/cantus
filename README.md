# Cantus

A desktop music library manager. Scan your folders, identify tracks by audio fingerprint, fix tags and file names with a preview and full undo, and share playlists as plain files.

Status: early development, no release yet.

## Features

- Scan folders into a SQLite library; unchanged files are skipped and moved files are recognized
- Edit tags on many files at once, with every change undoable from History
- Identify tracks by audio fingerprint (AcoustID) or by their tags (MusicBrainz), and review every change before it is written
- Organize files with an editable path template, previewed first; files are never overwritten
- Playlists as `.m3u8` and JSON; JSON files keep MusicBrainz ids, so they work across different folder layouts

Tag writes and moves are journaled, so an interrupted change is settled on the next start and undo restores files byte for byte.

Identify is off until enabled in Settings. Matching by audio needs a free [AcoustID](https://acoustid.org) API key; without one, Cantus searches by tags.

Planned: torrent downloads that feed into identify and organize, sync through rclone, URL import through yt-dlp.

## Build

Requires stable Rust.

```
cargo run --release
```

On Linux, install the audio and X11/Wayland development headers first, e.g. on Debian/Ubuntu:

```
sudo apt install libasound2-dev pkg-config libxkbcommon-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev
```

## License

Copyright (C) 2026 Aallyn. Licensed under GPL-3.0-only, see [LICENSE](LICENSE).
