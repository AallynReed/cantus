# Cantus

A desktop music library manager. Scan your folders, identify tracks by audio fingerprint, fix tags and file names with a preview and full undo, and share playlists as plain files.

Status: early development. Nothing is usable yet.

## Planned

- Library scan into SQLite
- Identification via AcoustID and MusicBrainz, with a preview of every change and undo
- Organizing files by an editable path template
- Playlists as `.m3u8` and JSON
- Built-in torrent downloads that feed straight into identify and organize
- Optional sync through rclone, and URL import through yt-dlp

## Build

Requires stable Rust.

```
cargo run --release
```

On Linux, install the X11/Wayland development headers first, e.g. on Debian/Ubuntu:

```
sudo apt install libxkbcommon-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev
```

## License

Copyright (C) 2026 Aallyn. Licensed under GPL-3.0-only, see [LICENSE](LICENSE).
