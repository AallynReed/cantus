//! Path templates: `{field}`, `{field:02}` zero-padding, `{a|b}` fallback, and `[...]` groups
//! that vanish when a field inside is empty. `/` separates folders.

use std::iter::Peekable;
use std::path::{Path, PathBuf};
use std::str::Chars;

use crate::Track;
use crate::paths::sanitize_component;

pub const DEFAULT_TEMPLATE: &str =
    "{albumartist|artist}/[{year} - ]{album}/[{disc}-]{track:02} {title}.{ext}";

const FIELDS: [&str; 9] = [
    "title",
    "artist",
    "albumartist",
    "album",
    "track",
    "disc",
    "year",
    "genre",
    "ext",
];
const MISSING: &str = "Unknown";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Template(Vec<Node>);

#[derive(Clone, Debug, PartialEq, Eq)]
enum Node {
    Text(String),
    Field { names: Vec<String>, width: usize },
    Group(Vec<Node>),
}

impl Template {
    pub fn parse(source: &str) -> Result<Self, String> {
        Ok(Self(parse_nodes(&mut source.chars().peekable(), false)?))
    }

    /// The track's path relative to its library root, safe on every filesystem.
    pub fn render(&self, track: &Track) -> PathBuf {
        let mut rendered = String::new();
        render_nodes(&self.0, track, &mut rendered);

        let mut path: PathBuf = rendered
            .split('/')
            .filter(|part| !part.trim().is_empty())
            .map(sanitize_component)
            .collect();
        let ext = extension(track.path.as_path());
        if !ext.is_empty() && path.extension().is_none_or(|e| e != ext.as_str()) {
            // A template without `.{ext}` must not strip the file's type.
            let name = format!(
                "{}.{ext}",
                path.file_name().unwrap_or_default().to_string_lossy()
            );
            path.set_file_name(sanitize_component(&name));
        }
        path
    }
}

fn parse_nodes(chars: &mut Peekable<Chars>, in_group: bool) -> Result<Vec<Node>, String> {
    let mut nodes = Vec::new();
    let mut text = String::new();
    while let Some(c) = chars.next() {
        match c {
            '{' | '[' | ']' if !text.is_empty() => {
                nodes.push(Node::Text(std::mem::take(&mut text)))
            }
            _ => {}
        }
        match c {
            '{' => nodes.push(parse_field(chars)?),
            '[' => nodes.push(Node::Group(parse_nodes(chars, true)?)),
            ']' if in_group => return Ok(nodes),
            ']' => return Err("']' without a matching '['".into()),
            '}' => return Err("'}' without a matching '{'".into()),
            c => text.push(c),
        }
    }
    if in_group {
        return Err("'[' is never closed".into());
    }
    if !text.is_empty() {
        nodes.push(Node::Text(text));
    }
    Ok(nodes)
}

fn parse_field(chars: &mut Peekable<Chars>) -> Result<Node, String> {
    let mut inner = String::new();
    loop {
        match chars.next() {
            Some('}') => break,
            Some(c) => inner.push(c),
            None => return Err("'{' is never closed".into()),
        }
    }
    let (names, width) = match inner.split_once(':') {
        Some((names, width)) => {
            let width = width
                .parse::<usize>()
                .ok()
                .filter(|w| *w <= 9)
                .ok_or_else(|| format!("bad padding in {{{inner}}}"))?;
            (names, width)
        }
        None => (inner.as_str(), 0),
    };
    let names: Vec<String> = names.split('|').map(|n| n.trim().to_lowercase()).collect();
    if let Some(unknown) = names.iter().find(|n| !FIELDS.contains(&n.as_str())) {
        return Err(format!("unknown field {{{unknown}}}"));
    }
    Ok(Node::Field { names, width })
}

/// Appends `nodes`; a missing field becomes "Unknown" outside a group.
fn render_nodes(nodes: &[Node], track: &Track, out: &mut String) {
    for node in nodes {
        match node {
            Node::Text(text) => out.push_str(text),
            Node::Field { names, width } => {
                out.push_str(&field_text(names, *width, track).unwrap_or(MISSING.to_owned()));
            }
            Node::Group(inner) => {
                if let Some(group) = render_group(inner, track) {
                    out.push_str(&group);
                }
            }
        }
    }
}

/// `None` when any field inside is empty.
fn render_group(nodes: &[Node], track: &Track) -> Option<String> {
    let mut out = String::new();
    for node in nodes {
        match node {
            Node::Text(text) => out.push_str(text),
            Node::Field { names, width } => out.push_str(&field_text(names, *width, track)?),
            Node::Group(inner) => out.push_str(&render_group(inner, track).unwrap_or_default()),
        }
    }
    Some(out)
}

/// The first non-empty named field, padded. Separators in values must not create folders.
fn field_text(names: &[String], width: usize, track: &Track) -> Option<String> {
    let value = names.iter().find_map(|name| field_value(name, track))?;
    let value = value.replace(['/', '\\'], "_");
    Some(format!("{value:0>width$}"))
}

fn field_value(name: &str, track: &Track) -> Option<String> {
    let m = &track.meta;
    let value = match name {
        "title" => m.title.clone().or_else(|| {
            track
                .path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
        }),
        "artist" => m.artist.clone(),
        "albumartist" => m.album_artist.clone(),
        "album" => m.album.clone(),
        "track" => m.track_no.map(|n| n.to_string()),
        "disc" => m.disc_no.map(|n| n.to_string()),
        "year" => m.year.map(|n| n.to_string()),
        "genre" => m.genre.clone(),
        "ext" => Some(extension(&track.path)),
        _ => None,
    };
    value.filter(|v| !v.trim().is_empty())
}

fn extension(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Metadata, TrackState};
    use proptest::prelude::*;
    use std::path::Component;

    fn track(meta: Metadata) -> Track {
        Track {
            id: 1,
            path: "/lib/old name.flac".into(),
            state: TrackState::Ok,
            error: None,
            meta,
        }
    }

    fn full() -> Metadata {
        Metadata {
            title: Some("Song".into()),
            artist: Some("Artist".into()),
            album: Some("Album".into()),
            track_no: Some(3),
            disc_no: Some(2),
            year: Some(1999),
            ..Default::default()
        }
    }

    fn render(source: &str, meta: Metadata) -> String {
        let path = Template::parse(source).unwrap().render(&track(meta));
        path.to_string_lossy().replace('\\', "/")
    }

    #[test]
    fn default_template_uses_fallbacks_groups_and_padding() {
        assert_eq!(
            render(DEFAULT_TEMPLATE, full()),
            "Artist/1999 - Album/2-03 Song.flac"
        );
        let sparse = Metadata {
            title: Some("Song".into()),
            album_artist: Some("Band".into()),
            ..Default::default()
        };
        assert_eq!(
            render(DEFAULT_TEMPLATE, sparse),
            "Band/Unknown/Unknown Song.flac"
        );
    }

    #[test]
    fn values_cannot_create_folders_or_escape() {
        let mut meta = full();
        meta.artist = Some("../../AC/DC".into());
        assert_eq!(
            render("{artist}/{title}.{ext}", meta),
            ".._.._AC_DC/Song.flac"
        );
    }

    #[test]
    fn missing_title_uses_the_file_name_and_extension_is_always_kept() {
        assert_eq!(render("{title}", Metadata::default()), "old name.flac");
    }

    #[test]
    fn bad_templates_are_rejected() {
        for bad in ["{nope}", "{title", "[{title}", "]", "}", "{track:x}"] {
            assert!(Template::parse(bad).is_err(), "{bad}");
        }
    }

    proptest! {
        #[test]
        fn rendered_paths_stay_relative(
            source in "(\\{title\\}|\\{artist\\}|/|\\.\\.|a|\\[|\\]| ){0,20}",
            title: String,
            artist: String,
        ) {
            let Ok(template) = Template::parse(&source) else { return Ok(()) };
            let meta = Metadata { title: Some(title), artist: Some(artist), ..Default::default() };
            let path = template.render(&track(meta));
            prop_assert!(path.components().all(|c| matches!(c, Component::Normal(_))), "{path:?}");
        }
    }
}
