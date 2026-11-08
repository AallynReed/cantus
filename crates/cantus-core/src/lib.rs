//! Library logic for Cantus. No UI types live here.

mod db;
mod edit;
mod journal;
mod organize;
mod paths;
mod scan;
mod tags;
mod template;

use std::path::PathBuf;

pub use db::{Library, Track, TrackState};
pub use edit::{Field, TagChange};
pub use journal::{BatchReport, ChangeSetSummary};
pub use organize::PlannedMove;
pub use scan::ScanReport;
pub use tags::Metadata;
pub use template::{DEFAULT_TEMPLATE, Template};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Db(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    InvalidInput(String),
    #[error("path is not valid UTF-8: {}", .0.display())]
    NonUtf8Path(PathBuf),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
