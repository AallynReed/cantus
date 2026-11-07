use std::io::{Cursor, Seek};

use lofty::config::WriteOptions;
use lofty::prelude::*;
use lofty::probe::Probe;
use lofty::tag::Tag;

use crate::{BatchReport, Error, Library, Metadata, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Field {
    Title,
    Artist,
    AlbumArtist,
    Album,
    TrackNo,
    DiscNo,
    Year,
    Genre,
}

impl Field {
    pub const ALL: [Self; 8] = [
        Self::Title,
        Self::Artist,
        Self::AlbumArtist,
        Self::Album,
        Self::TrackNo,
        Self::DiscNo,
        Self::Year,
        Self::Genre,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Title => "Title",
            Self::Artist => "Artist",
            Self::AlbumArtist => "Album artist",
            Self::Album => "Album",
            Self::TrackNo => "Track",
            Self::DiscNo => "Disc",
            Self::Year => "Year",
            Self::Genre => "Genre",
        }
    }

    pub fn get(self, m: &Metadata) -> Option<String> {
        match self {
            Self::Title => m.title.clone(),
            Self::Artist => m.artist.clone(),
            Self::AlbumArtist => m.album_artist.clone(),
            Self::Album => m.album.clone(),
            Self::TrackNo => m.track_no.map(|n| n.to_string()),
            Self::DiscNo => m.disc_no.map(|n| n.to_string()),
            Self::Year => m.year.map(|n| n.to_string()),
            Self::Genre => m.genre.clone(),
        }
    }

    fn key(self) -> ItemKey {
        match self {
            Self::Title => ItemKey::TrackTitle,
            Self::Artist => ItemKey::TrackArtist,
            Self::AlbumArtist => ItemKey::AlbumArtist,
            Self::Album => ItemKey::AlbumTitle,
            Self::TrackNo => ItemKey::TrackNumber,
            Self::DiscNo => ItemKey::DiscNumber,
            Self::Year => ItemKey::RecordingDate,
            Self::Genre => ItemKey::Genre,
        }
    }
}

/// Sets one field to a value, or clears it when the value is `None` or blank.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TagChange {
    pub field: Field,
    pub value: Option<String>,
}

impl TagChange {
    /// The value as it will be written, or an error for a non-numeric number field.
    pub fn normalized(&self) -> Result<Option<String>, String> {
        let Some(value) = self
            .value
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        else {
            return Ok(None);
        };
        let number = match self.field {
            Field::TrackNo | Field::DiscNo => value.parse::<u32>().map(u64::from),
            Field::Year => value.parse::<u16>().map(u64::from),
            _ => return Ok(Some(value.to_owned())),
        };
        number
            .map(|n| Some(n.to_string()))
            .map_err(|_| format!("{} must be a number", self.field.label()))
    }
}

impl Library {
    /// Writes `changes` to the files of `track_ids` as one undoable change set.
    pub fn edit_tags(
        &mut self,
        track_ids: &[i64],
        changes: &[TagChange],
    ) -> Result<(i64, BatchReport)> {
        let changes = changes
            .iter()
            .map(|c| Ok((c.field, c.normalized()?)))
            .collect::<Result<Vec<_>, String>>()
            .map_err(Error::InvalidInput)?;
        let files = self.track_paths(track_ids)?;
        let fields: Vec<_> = changes
            .iter()
            .map(|(f, _)| f.label().to_lowercase())
            .collect();
        let n = files.len();
        let description = format!(
            "Edit {} on {n} track{}",
            fields.join(", "),
            if n == 1 { "" } else { "s" }
        );
        self.rewrite_files(&description, files, |bytes| set_tags(bytes, &changes))
    }
}

fn set_tags(bytes: &[u8], changes: &[(Field, Option<String>)]) -> Result<Vec<u8>, String> {
    let mut file = Cursor::new(bytes.to_vec());
    let mut tagged = Probe::new(&mut file)
        .guess_file_type()
        .map_err(|e| e.to_string())?
        .read()
        .map_err(|e| e.to_string())?;
    // Edit the tag that reading uses, so other fields stay visible.
    let tag_type = tagged
        .primary_tag()
        .or_else(|| tagged.first_tag())
        .map_or(tagged.primary_tag_type(), Tag::tag_type);
    if tagged.tag(tag_type).is_none() {
        tagged.insert_tag(Tag::new(tag_type));
    }
    let tag = tagged
        .tag_mut(tag_type)
        .ok_or("this format cannot hold tags")?;

    for (field, value) in changes {
        if *field == Field::Year {
            tag.remove_key(ItemKey::Year);
        }
        match value {
            Some(value) => {
                tag.insert_text(field.key(), value.clone());
            }
            None => tag.remove_key(field.key()),
        }
    }

    file.rewind().map_err(|e| e.to_string())?;
    tag.save_to(&mut file, WriteOptions::default())
        .map_err(|e| e.to_string())?;
    Ok(file.into_inner())
}
