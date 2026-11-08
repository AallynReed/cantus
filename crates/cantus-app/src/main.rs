#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod editor;
mod import;
mod organize;
mod player;
mod playlists;
mod review;
mod settings;
mod sync;
mod table;

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use cantus_core::Library;

const DAILY_BACKUPS: usize = 7;

fn main() -> eframe::Result {
    let dirs = directories::ProjectDirs::from("", "", "Cantus");
    let data_dir = dirs.as_ref().map(|d| d.data_local_dir().to_path_buf());
    log_panics(data_dir.clone());
    let (db_path, library) = match open_library() {
        Ok(opened) => opened,
        Err(err) => {
            rfd::MessageDialog::new()
                .set_level(rfd::MessageLevel::Error)
                .set_title("Cantus")
                .set_description(format!("Cannot open the library: {err}"))
                .show();
            std::process::exit(1);
        }
    };
    back_up_in_background(db_path.clone());

    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_inner_size([1200.0, 800.0])
        .with_min_inner_size([800.0, 500.0]);
    if let Ok(icon) =
        eframe::icon_data::from_png_bytes(include_bytes!("../../../packaging/icon.png"))
    {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
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

fn open_library() -> Result<(PathBuf, Library), String> {
    let dirs = directories::ProjectDirs::from("", "", "Cantus").ok_or("no home directory found")?;
    let path = dirs.data_local_dir().join("library.db");
    let library = Library::open(&path).map_err(|e| e.to_string())?;
    Ok((path, library))
}
