# Changelog

## [0.1.0] - 2026-11-08

- Scan folders into a library. Unchanged files are skipped and moved files keep their place in playlists.
- Edit tags across many files at once. Every edit can be undone, and undo restores files byte for byte.
- Identify tracks by audio fingerprint or by their tags, then review each change before it is written.
- Embed cover art from the Cover Art Archive, and audition tracks with a 60-second preview.
- Organize files with an editable path template. Moves are previewed and never overwrite anything.
- Build playlists and share them as `.m3u8` or JSON. JSON playlists still work on a differently organized library.
- Download torrents with file selection. Finished downloads wait in an Inbox and can be imported without interrupting seeding.
- Import audio from web links with yt-dlp.
- Sync a library folder to cloud storage with rclone, always after a dry run.
- Interrupted changes are settled at the next start, and the library database is backed up daily.
