//! The Cantus app, started by the desktop binary or by Android.

mod app;
mod editor;
mod import;
mod organize;
mod platform;
mod player;
mod playlists;
mod review;
mod settings;
mod sync;
mod table;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use cantus_core::Library;

const DAILY_BACKUPS: usize = 7;

pub fn run_desktop() -> eframe::Result {
    let dirs = directories::ProjectDirs::from("", "", "Cantus");
    let data_dir = dirs.map(|d| d.data_local_dir().to_path_buf());
    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_inner_size([1200.0, 800.0])
        .with_min_inner_size([800.0, 500.0]);
    if let Ok(icon) =
        eframe::icon_data::from_png_bytes(include_bytes!("../../../packaging/icon.png"))
    {
        viewport = viewport.with_icon(icon);
    }
    run(
        data_dir,
        eframe::NativeOptions {
            viewport,
            ..Default::default()
        },
    )
}

#[cfg(target_os = "android")]
#[allow(unsafe_code)] // Android finds the entry point by this exact symbol name.
#[unsafe(no_mangle)]
fn android_main(app: platform::AndroidApp) {
    let data_dir = app.internal_data_path();
    platform::remember_app(app.clone());
    let options = eframe::NativeOptions {
        android_app: Some(app),
        ..Default::default()
    };
    if let Err(err) = run(data_dir, options) {
        platform::show_error(&format!("Cantus stopped: {err}"));
    }
}

fn run(data_dir: Option<PathBuf>, options: eframe::NativeOptions) -> eframe::Result {
    log_panics(data_dir.clone());
    let (db_path, library) = match open_library(data_dir.as_deref()) {
        Ok(opened) => opened,
        Err(err) => {
            platform::show_error(&format!("Cannot open the library: {err}"));
            std::process::exit(1);
        }
    };
    back_up_in_background(db_path.clone());

    let delete_data = Arc::new(AtomicBool::new(false));
    let delete_requested = delete_data.clone();
    let result = eframe::run_native(
        concat!("Cantus ", env!("CARGO_PKG_VERSION")),
        options,
        Box::new(|_| Ok(Box::new(app::App::new(db_path, library, delete_requested)))),
    );
    if delete_data.load(Ordering::Relaxed)
        && let Some(dir) = data_dir
    {
        // Only Cantus's own database, cache and settings. Music and downloads stay.
        let _ = fs::remove_dir_all(dir);
    }
    result
}

fn back_up_in_background(db_path: PathBuf) {
    thread::spawn(move || {
        let backups = db_path.with_file_name("backups");
        let done = Library::open(&db_path).and_then(|library| {
            fs::create_dir_all(&backups)?;
            library.daily_backup(&backups, DAILY_BACKUPS)
        });
        if let Err(err) = done {
            eprintln!("backup failed: {err}");
        }
    });
}

/// A windowed app has no console, so a crash leaves its reason in a local file.
fn log_panics(data_dir: Option<PathBuf>) {
    let Some(dir) = data_dir else { return };
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let written = fs::create_dir_all(&dir).and_then(|()| {
            let mut log = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("crash.log"))?;
            writeln!(log, "{info}")
        });
        if written.is_err() {
            eprintln!("cannot write the crash log");
        }
        default_hook(info);
    }));
}

fn open_library(data_dir: Option<&Path>) -> Result<(PathBuf, Library), String> {
    let path = data_dir.ok_or("no data folder found")?.join("library.db");
    let library = Library::open(&path).map_err(|e| e.to_string())?;
    Ok((path, library))
}
