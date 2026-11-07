//! Library logic for Cantus. No UI types live here.

mod db;
mod scan;
mod tags;

use std::path::PathBuf;

pub use db::{Library, Track, TrackState};
pub use scan::ScanReport;
pub use tags::Metadata;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Db(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("path is not valid UTF-8: {}", .0.display())]
    NonUtf8Path(PathBuf),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
