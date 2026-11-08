use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

use cantus_core::{Library, TorrentConfig, tool_version};
use eframe::egui::{self, Ui};

use crate::platform;

const ONLINE_LOOKUPS: &str = "online_lookups";
const ACOUSTID_KEY: &str = "acoustid_key";
const TORRENTS: &str = "torrents";
const DOWNLOAD_DIR: &str = "download_dir";
const PROXY: &str = "proxy";
const DOWN_LIMIT: &str = "download_limit_kib";
const UP_LIMIT: &str = "upload_limit_kib";
const TOOLS: [(&str, &str); 3] = [
    ("yt-dlp", "--version"),
    ("ffmpeg", "-version"),
    ("rclone", "version"),
];

/// A key baked in at build time, for official builds.
const BUILT_IN_ACOUSTID_KEY: Option<&str> = option_env!("CANTUS_ACOUSTID_KEY");

pub struct Settings {
    pub online_lookups: bool,
    acoustid_key: String,
    pub torrents: bool,
    pub download_dir: PathBuf,
    proxy: String,
    down_limit_kib: u32,
    up_limit_kib: u32,
    tools: Option<Vec<String>>,
}

pub enum Change {
    None,
    Torrents(bool),
    DeleteAllData,
    Failed(String),
}

impl Settings {
    pub fn load(library: &Library) -> Self {
        let read = |key| library.setting(key).ok().flatten();
        Self {
            online_lookups: read(ONLINE_LOOKUPS).as_deref() == Some("on"),
            acoustid_key: read(ACOUSTID_KEY).unwrap_or_default(),
            torrents: read(TORRENTS).as_deref() == Some("on"),
            download_dir: read(DOWNLOAD_DIR)
                .map_or_else(platform::default_download_dir, PathBuf::from),
            proxy: read(PROXY).unwrap_or_default(),
            down_limit_kib: read(DOWN_LIMIT).and_then(|v| v.parse().ok()).unwrap_or(0),
            up_limit_kib: read(UP_LIMIT).and_then(|v| v.parse().ok()).unwrap_or(0),
            tools: None,
        }
    }

    /// The engine's settings, using `state_dir` for its resume data.
    pub fn torrent_config(&self, state_dir: PathBuf) -> TorrentConfig {
        TorrentConfig {
            download_dir: self.download_dir.clone(),
            state_dir,
            listen: (std::net::Ipv4Addr::UNSPECIFIED, 0).into(),
            dht: true,
            proxy: Some(self.proxy.trim().to_owned()).filter(|p| !p.is_empty()),
            download_limit: NonZeroU32::new(self.down_limit_kib.saturating_mul(1024)),
            upload_limit: NonZeroU32::new(self.up_limit_kib.saturating_mul(1024)),
            peers: Vec::new(),
        }
    }

    pub fn show(&mut self, ui: &mut Ui, library: &Library) -> Change {
        let mut result = Ok(());
        let mut change = Change::None;
        ui.heading("Online lookups");
        ui.add_space(4.0);
        if ui
            .checkbox(&mut self.online_lookups, "Allow online lookups")
            .changed()
        {
            let value = if self.online_lookups { "on" } else { "off" };
            result = library.set_setting(ONLINE_LOOKUPS, value);
        }
        ui.label(
            "Identify sends audio fingerprints, durations and tag text to AcoustID and \
             MusicBrainz, and fetches cover art from the Cover Art Archive. It never sends file \
             paths or anything about you. Cantus has no servers of its own and no telemetry.",
        );
        ui.add_space(12.0);

        let label = ui.label("AcoustID API key");
        let edit = ui
            .add(egui::TextEdit::singleline(&mut self.acoustid_key).desired_width(280.0))
            .labelled_by(label.id);
        if edit.changed() {
            result = library.set_setting(ACOUSTID_KEY, self.acoustid_key.trim());
        }
        ui.small(
            "Needed to match by audio; without one, Cantus searches by the existing tags. \
             Get a free key at acoustid.org.",
        );
        ui.add_space(16.0);

        ui.heading("Torrents");
        ui.add_space(4.0);
        if ui
            .checkbox(&mut self.torrents, "Enable the torrent engine")
            .changed()
        {
            let value = if self.torrents { "on" } else { "off" };
            result = library.set_setting(TORRENTS, value);
            change = Change::Torrents(self.torrents);
        }
        ui.label("While it runs, other peers can see your IP address. Changes below apply the next time the engine starts.");
        ui.horizontal(|ui| {
            ui.label(format!("Download folder: {}", self.download_dir.display()));
            if ui.small_button("Change…").clicked()
                && let Some(dir) = platform::pick_folder().filter(|_| platform::DESKTOP)
            {
                result = library.set_setting(DOWNLOAD_DIR, &dir.to_string_lossy());
                self.download_dir = dir;
            }
        });
        let label = ui.label("SOCKS5 proxy, like socks5://host:port");
        let proxy = ui
            .add(egui::TextEdit::singleline(&mut self.proxy).desired_width(280.0))
            .labelled_by(label.id);
        if proxy.changed() {
            result = library.set_setting(PROXY, self.proxy.trim());
        }
        ui.horizontal(|ui| {
            for (name, key, value) in [
                (
                    "Download limit, KiB/s (0 = none)",
                    DOWN_LIMIT,
                    &mut self.down_limit_kib,
                ),
                (
                    "Upload limit, KiB/s (0 = none)",
                    UP_LIMIT,
                    &mut self.up_limit_kib,
                ),
            ] {
                let label = ui.label(name);
                let speed = ui.add(egui::DragValue::new(value)).labelled_by(label.id);
                if speed.changed() {
                    result = library.set_setting(key, &value.to_string());
                }
            }
        });
        ui.add_space(16.0);

        ui.heading("External tools");
        ui.add_space(4.0);
        ui.label(
            "Web links need yt-dlp and ffmpeg, and Sync needs rclone, installed on your PATH.",
        );
        if ui.button("Check tools").clicked() {
            self.tools = Some(
                TOOLS
                    .iter()
                    .map(|(name, flag)| match tool_version(Path::new(name), flag) {
                        Some(version) => format!("{name}: {version}"),
                        None => format!("{name}: not found"),
                    })
                    .collect(),
            );
        }
        for line in self.tools.iter().flatten() {
            ui.label(line);
        }
        ui.add_space(16.0);

        ui.heading("Your data");
        ui.add_space(4.0);
        ui.label("Everything Cantus stores about your library stays on this computer.");
        if ui.button("Delete all Cantus data…").clicked() {
            change = Change::DeleteAllData;
        }

        match result {
            Ok(()) => change,
            Err(err) => Change::Failed(format!("Cannot save settings: {err}")),
        }
    }
}

/// The key to use for lookups: the user's, or the one built in.
pub fn acoustid_key(library: &Library) -> Option<String> {
    library
        .setting(ACOUSTID_KEY)
        .ok()
        .flatten()
        .filter(|k| !k.is_empty())
        .or(BUILT_IN_ACOUSTID_KEY.map(str::to_owned))
}
