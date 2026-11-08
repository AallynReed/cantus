//! Audio decoding for fingerprints and previews.

use std::fs::File;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;

use data_encoding::BASE64URL_NOPAD;
use rusty_chromaprint::{Configuration, FingerprintCompressor, Fingerprinter};
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as DecodeError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

/// AcoustID matches on the opening of a track; decoding more only costs time.
const FINGERPRINT_SECONDS: u32 = 120;

/// Decoded interleaved audio.
pub struct Pcm {
    pub sample_rate: u32,
    pub channels: u16,
    pub samples: Vec<f32>,
}

/// Decodes up to `max_seconds` of audio for playback.
pub fn decode_preview(path: &Path, max_seconds: u32) -> Result<Pcm, String> {
    let mut pcm = Pcm {
        sample_rate: 0,
        channels: 0,
        samples: Vec::new(),
    };
    let mut chunk = Vec::new();
    decode(
        path,
        max_seconds,
        &mut chunk,
        |rate, channels, samples: &[f32]| {
            pcm.sample_rate = rate;
            pcm.channels = channels as u16;
            pcm.samples.extend_from_slice(samples);
            Ok(())
        },
    )?;
    Ok(pcm)
}

/// The compressed, URL-safe Chromaprint fingerprint AcoustID expects.
pub(crate) fn fingerprint(path: &Path) -> Result<String, String> {
    let config = Configuration::preset_test2();
    let mut printer = Fingerprinter::new(&config);
    let mut started = false;
    let mut chunk = Vec::new();
    decode(
        path,
        FINGERPRINT_SECONDS,
        &mut chunk,
        |rate, channels, samples: &[i16]| {
            if !started {
                printer
                    .start(rate, channels)
                    .map_err(|e| format!("cannot fingerprint this audio: {e}"))?;
                started = true;
            }
            printer.consume(samples);
            Ok(())
        },
    )?;
    printer.finish();

    let raw = printer.fingerprint();
    if raw.is_empty() {
        return Err("audio is too short to fingerprint".into());
    }
    Ok(BASE64URL_NOPAD.encode(&FingerprintCompressor::from(&config).compress(raw)))
}

/// Runs the decode loop. Parsing untrusted files, so a decoder panic is an ordinary failure.
fn decode<S>(
    path: &Path,
    max_seconds: u32,
    chunk: &mut Vec<S>,
    mut sink: impl FnMut(u32, u32, &[S]) -> Result<(), String>,
) -> Result<(), String>
where
    S: symphonia::core::audio::sample::Sample
        + symphonia::core::audio::conv::FromSample<u8>
        + symphonia::core::audio::conv::FromSample<u16>
        + symphonia::core::audio::conv::FromSample<symphonia::core::audio::sample::u24>
        + symphonia::core::audio::conv::FromSample<u32>
        + symphonia::core::audio::conv::FromSample<i8>
        + symphonia::core::audio::conv::FromSample<i16>
        + symphonia::core::audio::conv::FromSample<symphonia::core::audio::sample::i24>
        + symphonia::core::audio::conv::FromSample<i32>
        + symphonia::core::audio::conv::FromSample<f32>
        + symphonia::core::audio::conv::FromSample<f64>,
{
    panic::catch_unwind(AssertUnwindSafe(|| {
        decode_inner(path, max_seconds, chunk, &mut sink)
    }))
    .unwrap_or_else(|_| Err("the decoder could not read this file".into()))
}

fn decode_inner<S>(
    path: &Path,
    max_seconds: u32,
    chunk: &mut Vec<S>,
    sink: &mut impl FnMut(u32, u32, &[S]) -> Result<(), String>,
) -> Result<(), String>
where
    S: symphonia::core::audio::sample::Sample
        + symphonia::core::audio::conv::FromSample<u8>
        + symphonia::core::audio::conv::FromSample<u16>
        + symphonia::core::audio::conv::FromSample<symphonia::core::audio::sample::u24>
        + symphonia::core::audio::conv::FromSample<u32>
        + symphonia::core::audio::conv::FromSample<i8>
        + symphonia::core::audio::conv::FromSample<i16>
        + symphonia::core::audio::conv::FromSample<symphonia::core::audio::sample::i24>
        + symphonia::core::audio::conv::FromSample<i32>
        + symphonia::core::audio::conv::FromSample<f32>
        + symphonia::core::audio::conv::FromSample<f64>,
{
    let file = File::open(path).map_err(|e| e.to_string())?;
    let stream = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            stream,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| format!("unsupported audio format: {e}"))?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or("no audio track")?;
    let track_id = track.id;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or("no audio codec information")?;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(params, &AudioDecoderOptions::default())
        .map_err(|e| format!("unsupported audio codec: {e}"))?;

    let mut decoded_frames = 0u64;
    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) | Err(DecodeError::ResetRequired) => return Ok(()),
            Err(DecodeError::IoError(_)) => return Ok(()),
            Err(err) => return Err(err.to_string()),
        };
        if packet.track_id != track_id {
            continue;
        }
        let audio = match decoder.decode(&packet) {
            Ok(audio) => audio,
            Err(DecodeError::IoError(_) | DecodeError::DecodeError(_)) => continue,
            Err(err) => return Err(err.to_string()),
        };
        let rate = audio.spec().rate();
        let channels = audio.spec().channels().count() as u32;
        chunk.clear();
        audio.copy_to_vec_interleaved(chunk);
        sink(rate, channels, chunk)?;
        decoded_frames += audio.frames() as u64;
        if decoded_frames >= u64::from(max_seconds) * u64::from(rate) {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    #[test]
    fn fingerprint_is_url_safe_chromaprint_data() {
        let encoded = fingerprint(&fixture("long.flac")).unwrap();
        let bytes = BASE64URL_NOPAD.decode(encoded.as_bytes()).unwrap();
        assert_eq!(
            bytes[0],
            Configuration::preset_test2().id(),
            "algorithm id comes first"
        );
        assert!(bytes.len() > 4);
    }

    #[test]
    fn every_decodable_format_gives_samples() {
        for ext in ["aiff", "flac", "m4a", "mp3", "ogg", "wav"] {
            let pcm = decode_preview(&fixture(&format!("tone.{ext}")), 60).unwrap();
            assert_eq!(pcm.channels, 1, "{ext}");
            assert!(pcm.samples.len() > 1000, "{ext}: {}", pcm.samples.len());
        }
    }

    #[test]
    fn undecodable_formats_and_garbage_fail_cleanly() {
        assert!(decode_preview(&fixture("tone.opus"), 60).is_err());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("garbage.mp3");
        std::fs::write(&path, vec![0x55; 4096]).unwrap();
        assert!(fingerprint(&path).is_err());
    }

    #[test]
    fn preview_stops_at_the_limit() {
        let pcm = decode_preview(&fixture("long.flac"), 1).unwrap();
        let seconds = pcm.samples.len() as f32 / pcm.sample_rate as f32;
        assert!(seconds < 1.5, "{seconds}");
    }
}
