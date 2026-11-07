use std::borrow::Cow;
use std::path::Path;

use lofty::error::FileParseError;
use lofty::prelude::*;

/// Tags and audio properties read from one file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Metadata {
    pub duration_ms: Option<u32>,
    pub bitrate: Option<u32>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u8>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub track_no: Option<u32>,
    pub disc_no: Option<u32>,
    pub year: Option<u16>,
    pub genre: Option<String>,
}

pub fn read(path: &Path) -> Result<Metadata, FileParseError> {
    let file = lofty::read_from_path(path)?;
    let props = file.properties();
    let tag = file.primary_tag().or_else(|| file.first_tag());
    let text =
        |get: fn(&lofty::tag::Tag) -> Option<Cow<'_, str>>| tag.and_then(get).map(Cow::into_owned);

    Ok(Metadata {
        duration_ms: u32::try_from(props.duration().as_millis()).ok(),
        bitrate: props.audio_bitrate(),
        sample_rate: props.sample_rate(),
        channels: props.channels(),
        title: text(|t| t.title()),
        artist: text(|t| t.artist()),
        album_artist: tag
            .and_then(|t| t.get_string(ItemKey::AlbumArtist))
            .map(str::to_owned),
        album: text(|t| t.album()),
        track_no: tag.and_then(|t| t.track()),
        disc_no: tag.and_then(|t| t.disk()),
        year: tag.and_then(|t| t.date()).map(|d| d.year),
        genre: text(|t| t.genre()),
    })
}
