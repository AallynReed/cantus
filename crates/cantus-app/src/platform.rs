//! What differs between desktop and Android: file dialogs, folders and the on-screen keyboard.

pub use imp::*;

#[cfg(not(target_os = "android"))]
mod imp {
    use std::path::PathBuf;

    /// Desktop has native file dialogs and can run yt-dlp and rclone.
    pub const DESKTOP: bool = true;

    pub fn show_error(message: &str) {
        rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Error)
            .set_title("Cantus")
            .set_description(message)
            .show();
    }

    pub fn pick_folder() -> Option<PathBuf> {
        rfd::FileDialog::new().pick_folder()
    }

    pub fn pick_torrent() -> Option<PathBuf> {
        rfd::FileDialog::new()
            .add_filter("Torrent", &["torrent"])
            .pick_file()
    }

    pub fn pick_playlist() -> Option<PathBuf> {
        rfd::FileDialog::new()
            .add_filter("Playlists", &["m3u8", "m3u", "json"])
            .pick_file()
    }

    pub fn save_playlist() -> Option<PathBuf> {
        rfd::FileDialog::new()
            .add_filter("M3U8 playlist", &["m3u8"])
            .add_filter("Cantus playlist (keeps MusicBrainz ids)", &["json"])
            .save_file()
    }

    pub fn default_download_dir() -> PathBuf {
        directories::UserDirs::new()
            .and_then(|dirs| dirs.download_dir().map(PathBuf::from))
            .unwrap_or_else(std::env::temp_dir)
            .join("Cantus")
    }

    pub fn sync_keyboard(_wanted: bool) {}
}

#[cfg(target_os = "android")]
mod imp {
    use std::path::PathBuf;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, Ordering};

    pub use android_activity::AndroidApp;

    pub const DESKTOP: bool = false;
    const MUSIC_FOLDER: &str = "/storage/emulated/0/Music";
    const DOWNLOAD_FOLDER: &str = "/storage/emulated/0/Download/Cantus";

    static APP: OnceLock<AndroidApp> = OnceLock::new();
    static KEYBOARD_SHOWN: AtomicBool = AtomicBool::new(false);

    pub fn remember_app(app: AndroidApp) {
        let _ = APP.set(app);
    }

    pub fn show_error(message: &str) {
        eprintln!("{message}");
    }

    /// There is no folder dialog here; the shared Music folder is the library.
    pub fn pick_folder() -> Option<PathBuf> {
        Some(PathBuf::from(MUSIC_FOLDER))
    }

    pub fn pick_torrent() -> Option<PathBuf> {
        None
    }

    pub fn pick_playlist() -> Option<PathBuf> {
        None
    }

    pub fn save_playlist() -> Option<PathBuf> {
        None
    }

    pub fn default_download_dir() -> PathBuf {
        PathBuf::from(DOWNLOAD_FOLDER)
    }

    /// Android doesn't raise the keyboard by itself for a native activity.
    pub fn sync_keyboard(wanted: bool) {
        if KEYBOARD_SHOWN.swap(wanted, Ordering::Relaxed) == wanted {
            return;
        }
        if let Some(app) = APP.get() {
            if wanted {
                app.show_soft_input(false);
            } else {
                app.hide_soft_input(false);
            }
        }
    }
}
