#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod table;

use std::path::PathBuf;

use cantus_core::Library;

fn main() -> eframe::Result {
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
    eframe::run_native(
        concat!("Cantus ", env!("CARGO_PKG_VERSION")),
        eframe::NativeOptions::default(),
        Box::new(|_| Ok(Box::new(app::App::new(db_path, library)))),
    )
}

fn open_library() -> Result<(PathBuf, Library), String> {
    let dirs = directories::ProjectDirs::from("", "", "Cantus").ok_or("no home directory found")?;
    let path = dirs.data_local_dir().join("library.db");
    let library = Library::open(&path).map_err(|e| e.to_string())?;
    Ok((path, library))
}
