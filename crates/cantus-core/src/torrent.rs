//! The torrent engine. librqbit stays behind these types so a backend swap touches one file.

use std::collections::{BTreeSet, HashSet};
use std::fmt::Display;
use std::fs;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::{Component, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use librqbit::api::TorrentIdOrHash;
use librqbit::limits::LimitsConfig;
use librqbit::{
    AddTorrent, AddTorrentOptions, ConnectionOptions, DhtSessionConfig, ListenerMode,
    ListenerOptions, ManagedTorrent, Session, SessionOptions, SessionPersistenceConfig,
    TorrentStatsState,
};
use tokio::runtime::Runtime;
use tokio::time::timeout;

use crate::{Error, Result};

/// How long to look for a magnet link's metadata before giving up.
const ADD_TIMEOUT: Duration = Duration::from_secs(300);
const BYTES_PER_MIB: f64 = 1024.0 * 1024.0;

pub struct TorrentConfig {
    pub download_dir: PathBuf,
    /// Where the session remembers its torrents between runs.
    pub state_dir: PathBuf,
    pub listen: SocketAddr,
    pub dht: bool,
    /// `socks5://host:port`, for all outgoing peer connections.
    pub proxy: Option<String>,
    pub download_limit: Option<NonZeroU32>,
    pub upload_limit: Option<NonZeroU32>,
    /// Peers to try for every torrent, besides those trackers and the DHT find.
    pub peers: Vec<SocketAddr>,
}

pub enum TorrentSource {
    Magnet(String),
    File(Vec<u8>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TorrentId(pub usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TorrentState {
    Checking,
    Downloading,
    Seeding,
    Paused,
    Failed,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TorrentStatus {
    pub id: TorrentId,
    pub info_hash: String,
    pub name: String,
    pub state: TorrentState,
    pub done_bytes: u64,
    pub total_bytes: u64,
    pub down_bytes_per_sec: f64,
    pub up_bytes_per_sec: f64,
    pub finished: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TorrentFile {
    pub index: usize,
    /// Relative to the torrent's output folder.
    pub path: PathBuf,
    /// Absolute location on disk.
    pub location: PathBuf,
    pub len: u64,
    pub selected: bool,
}

pub struct Torrents {
    session: Arc<Session>,
    peers: Vec<SocketAddr>,
    runtime: Runtime,
}

fn failed(err: impl Display) -> Error {
    Error::InvalidInput(format!("torrent error: {err}"))
}

impl Torrents {
    pub fn start(config: TorrentConfig) -> Result<Self> {
        fs::create_dir_all(&config.download_dir)?;
        fs::create_dir_all(&config.state_dir)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let options = SessionOptions {
            dht: config.dht.then(DhtSessionConfig::default),
            disable_local_service_discovery: !config.dht,
            disable_trackers: !config.dht,
            fastresume: true,
            persistence: Some(SessionPersistenceConfig::Json {
                folder: Some(config.state_dir),
            }),
            listen: Some(ListenerOptions {
                mode: ListenerMode::TcpAndUtp,
                listen_addr: config.listen,
                ..Default::default()
            }),
            connect: Some(ConnectionOptions {
                proxy_url: config.proxy,
                ..Default::default()
            }),
            ratelimits: LimitsConfig {
                download_bps: config.download_limit,
                upload_bps: config.upload_limit,
            },
            ..Default::default()
        };
        let session = runtime
            .block_on(Session::new_with_opts(config.download_dir, options))
            .map_err(failed)?;
        Ok(Self {
            session,
            peers: config.peers,
            runtime,
        })
    }

    /// Adds a torrent, paused so the user can pick files first. A magnet link blocks until
    /// its metadata arrives, so call this off the UI thread.
    pub fn add(&self, source: TorrentSource) -> Result<TorrentId> {
        let add = match source {
            TorrentSource::Magnet(url) if url.starts_with("magnet:") => AddTorrent::from_url(url),
            TorrentSource::Magnet(_) => {
                return Err(Error::InvalidInput("that is not a magnet link".into()));
            }
            TorrentSource::File(bytes) => AddTorrent::from_bytes(bytes),
        };
        let options = AddTorrentOptions {
            paused: true,
            initial_peers: (!self.peers.is_empty()).then(|| self.peers.clone()),
            ..Default::default()
        };
        let response = self
            .runtime
            .block_on(async {
                timeout(ADD_TIMEOUT, self.session.add_torrent(add, Some(options))).await
            })
            .map_err(|_| failed("timed out looking for the torrent's metadata"))?
            .map_err(failed)?;
        let handle = response
            .into_handle()
            .ok_or_else(|| failed("the torrent was not added"))?;
        // Files can only be selected once the engine has finished setting the torrent up.
        self.runtime
            .block_on(handle.wait_until_initialized())
            .map_err(failed)?;
        Ok(TorrentId(handle.id()))
    }

    pub fn list(&self) -> Vec<TorrentStatus> {
        self.session.with_torrents(|torrents| {
            let mut all: Vec<_> = torrents.map(|(id, t)| status(id, t)).collect();
            all.sort_by_key(|s| s.id.0);
            all
        })
    }

    /// The torrent's files. Empty until its metadata is known.
    pub fn files(&self, id: TorrentId) -> Vec<TorrentFile> {
        let Some(torrent) = self.handle(id) else {
            return Vec::new();
        };
        let selected = torrent
            .only_files()
            .map(|f| f.into_iter().collect::<HashSet<_>>());
        let output = torrent.output_folder().to_path_buf();
        torrent
            .with_metadata(|meta| {
                meta.file_infos
                    .iter()
                    .enumerate()
                    .filter(|(_, f)| is_contained(&f.relative_filename))
                    .map(|(index, f)| TorrentFile {
                        index,
                        path: f.relative_filename.clone(),
                        location: output.join(&f.relative_filename),
                        len: f.len,
                        selected: selected.as_ref().is_none_or(|s| s.contains(&index)),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn select_files(&self, id: TorrentId, indexes: &BTreeSet<usize>) -> Result<()> {
        let torrent = self.require(id)?;
        let chosen: HashSet<usize> = indexes.iter().copied().collect();
        self.runtime
            .block_on(self.session.update_only_files(&torrent, &chosen))
            .map_err(failed)
    }

    pub fn resume(&self, id: TorrentId) -> Result<()> {
        let torrent = self.require(id)?;
        self.runtime
            .block_on(self.session.unpause(&torrent))
            .map_err(failed)
    }

    pub fn pause(&self, id: TorrentId) -> Result<()> {
        let torrent = self.require(id)?;
        self.runtime
            .block_on(self.session.pause(&torrent))
            .map_err(failed)
    }

    pub fn remove(&self, id: TorrentId, delete_files: bool) -> Result<()> {
        self.runtime
            .block_on(self.session.delete(TorrentIdOrHash::Id(id.0), delete_files))
            .map_err(failed)
    }

    fn handle(&self, id: TorrentId) -> Option<Arc<ManagedTorrent>> {
        self.session.get(TorrentIdOrHash::Id(id.0))
    }

    fn require(&self, id: TorrentId) -> Result<Arc<ManagedTorrent>> {
        self.handle(id).ok_or_else(|| failed("no such torrent"))
    }
}

impl Drop for Torrents {
    fn drop(&mut self) {
        self.runtime.block_on(self.session.stop());
    }
}

fn status(id: usize, torrent: &ManagedTorrent) -> TorrentStatus {
    let stats = torrent.stats();
    let state = match stats.state {
        TorrentStatsState::Initializing { .. } => TorrentState::Checking,
        TorrentStatsState::Paused => TorrentState::Paused,
        TorrentStatsState::Live if stats.finished => TorrentState::Seeding,
        TorrentStatsState::Live => TorrentState::Downloading,
        TorrentStatsState::Error => TorrentState::Failed,
    };
    let (down, up) = stats.live.as_ref().map_or((0.0, 0.0), |live| {
        (
            live.download_speed.mbps * BYTES_PER_MIB,
            live.upload_speed.mbps * BYTES_PER_MIB,
        )
    });
    TorrentStatus {
        id: TorrentId(id),
        info_hash: torrent.info_hash().as_string(),
        name: torrent.name().unwrap_or_else(|| "Unknown".to_owned()),
        state,
        done_bytes: stats.progress_bytes,
        total_bytes: stats.total_bytes,
        down_bytes_per_sec: down,
        up_bytes_per_sec: up,
        finished: stats.finished,
        error: stats.error,
    }
}

/// A torrent's file names are untrusted: none may point outside its folder.
fn is_contained(path: &std::path::Path) -> bool {
    path.components().all(|c| matches!(c, Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn escaping_file_names_are_rejected() {
        assert!(is_contained(Path::new("album/01 song.flac")));
        assert!(!is_contained(Path::new("../evil.exe")));
        assert!(!is_contained(Path::new("album/../../evil.exe")));
        assert!(!is_contained(&std::path::absolute("x").unwrap()));
    }
}
