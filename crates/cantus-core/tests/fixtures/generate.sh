#!/usr/bin/env bash
# Regenerates the tagged half-second test tones. Needs ffmpeg with libmp3lame, libvorbis and libopus.
set -euo pipefail
cd "$(dirname "$0")"

tags=(-metadata title="Test Tone" -metadata artist="Test Artist" -metadata album_artist="Test Album Artist"
      -metadata album="Test Album" -metadata track=3 -metadata disc=2 -metadata date=2024 -metadata genre="Test")

encode() { ffmpeg -v error -y -f lavfi -i "sine=frequency=440:duration=0.5:sample_rate=$1" -ac 1 "${tags[@]}" "${@:2}"; }

encode 8000 -c:a pcm_s16le tone.wav
encode 8000 -c:a pcm_s16be -write_id3v2 1 tone.aiff
encode 8000 -c:a flac tone.flac
# APE tags use their own key names.
encode 8000 -c:a wavpack -metadata Year=2024 -metadata "Album Artist=Test Album Artist" tone.wv
encode 22050 -c:a libmp3lame -b:a 32k tone.mp3
encode 22050 -c:a libvorbis -q:a 0 tone.ogg
encode 48000 -c:a libopus -b:a 16k tone.opus
encode 22050 -c:a aac -b:a 32k tone.m4a
ffmpeg -v error -y -f lavfi -i "sine=frequency=440:duration=4:sample_rate=11025" -ac 1 "${tags[@]}" -c:a flac long.flac
