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

    /// Where to export a playlist: none if the user cancels.
    pub fn save_playlist(_name: &str) -> Vec<PathBuf> {
        rfd::FileDialog::new()
            .add_filter("M3U8 playlist", &["m3u8"])
            .add_filter("Cantus playlist (keeps MusicBrainz ids)", &["json"])
            .save_file()
            .into_iter()
            .collect()
    }

    /// Only Android lists files itself, having no file dialogs.
    pub fn playlist_files() -> Vec<PathBuf> {
        Vec::new()
    }

    pub fn torrent_files() -> Vec<PathBuf> {
        Vec::new()
    }

    pub fn has_file_access() -> bool {
        true
    }

    pub fn request_file_access() {}

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
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, Ordering};

    pub use android_activity::AndroidApp;
    use jni::objects::{JObject, JValue};
    use jni::{Env, JavaVM, jni_sig, jni_str};

    pub const DESKTOP: bool = false;
    const MUSIC_FOLDER: &str = "/storage/emulated/0/Music";
    const DOWNLOAD_FOLDER: &str = "/storage/emulated/0/Download/Cantus";
    const DOWNLOADS: &str = "/storage/emulated/0/Download";
    const PLAYLIST_FOLDER: &str = "/storage/emulated/0/Music/Playlists";

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

    /// Both formats, so the playlist works in other apps and keeps its MusicBrainz ids.
    pub fn save_playlist(name: &str) -> Vec<PathBuf> {
        let stem: String = name
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || " -_".contains(c) {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let base = Path::new(PLAYLIST_FOLDER).join(stem);
        let _ = fs::create_dir_all(PLAYLIST_FOLDER);
        vec![base.with_extension("m3u8"), base.with_extension("json")]
    }

    pub fn playlist_files() -> Vec<PathBuf> {
        files_in(PLAYLIST_FOLDER, &["m3u8", "m3u", "json"])
    }

    pub fn torrent_files() -> Vec<PathBuf> {
        files_in(DOWNLOADS, &["torrent"])
    }

    fn files_in(folder: &str, extensions: &[&str]) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(folder) else {
            return Vec::new();
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(|entry| Some(entry.ok()?.path()))
            .filter(|path| {
                path.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| extensions.iter().any(|x| x.eq_ignore_ascii_case(e)))
            })
            .collect();
        files.sort();
        files
    }

    /// Runs `work` with a JNI environment and the current activity.
    fn with_activity<T>(
        work: impl FnOnce(&mut Env, &JObject) -> jni::errors::Result<T>,
    ) -> Option<T> {
        let app = APP.get()?;
        #[allow(unsafe_code)] // The app hands out pointers valid for the life of the activity.
        let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
        vm.attach_current_thread_for_scope(|env| {
            #[allow(unsafe_code)] // See above; this reference is only used inside this call.
            let activity = unsafe { JObject::from_raw(env, app.activity_as_ptr().cast()) };
            work(env, &activity)
        })
        .ok()
    }

    /// Scanning the shared Music folder needs Android's "All files access".
    pub fn has_file_access() -> bool {
        with_activity(|env, _| {
            env.call_static_method(
                jni_str!("android/os/Environment"),
                jni_str!("isExternalStorageManager"),
                jni_sig!("()Z"),
                &[],
            )?
            .z()
        })
        .unwrap_or(false)
    }

    /// Opens Android's settings page for granting that access.
    pub fn request_file_access() {
        let opened = with_activity(|env, activity| {
            let action =
                env.new_string("android.settings.MANAGE_APP_ALL_FILES_ACCESS_PERMISSION")?;
            let package = env.new_string("package:org.cantus.app")?;
            let uri = env
                .call_static_method(
                    jni_str!("android/net/Uri"),
                    jni_str!("parse"),
                    jni_sig!("(Ljava/lang/String;)Landroid/net/Uri;"),
                    &[JValue::Object(&package)],
                )?
                .l()?;
            let intent = env.new_object(
                jni_str!("android/content/Intent"),
                jni_sig!("(Ljava/lang/String;Landroid/net/Uri;)V"),
                &[JValue::Object(&action), JValue::Object(&uri)],
            )?;
            env.call_method(
                activity,
                jni_str!("startActivity"),
                jni_sig!("(Landroid/content/Intent;)V"),
                &[JValue::Object(&intent)],
            )?;
            Ok(())
        });
        if opened.is_none() {
            show_error("Could not open Android's file access settings");
        }
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
