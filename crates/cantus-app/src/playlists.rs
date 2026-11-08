use std::path::PathBuf;

use cantus_core::{Entry, Library, PlaylistSummary};
use eframe::egui::{self, RichText, Ui};

const LIST_WIDTH: f32 = 220.0;
const ROW_HEIGHT: f32 = 24.0;

/// The Playlists tab. Its edits are small, so they run directly on the UI's connection.
#[derive(Default)]
pub struct Playlists {
    pub all: Vec<PlaylistSummary>,
    selected: Option<i64>,
    entries: Vec<Entry>,
    new_name: String,
}

enum Edit {
    Move(usize, usize),
    Remove(usize),
}

impl Playlists {
    pub fn refresh(&mut self, library: &Library) -> cantus_core::Result<()> {
        self.all = library.playlists()?;
        self.selected = self
            .selected
            .filter(|id| self.all.iter().any(|p| p.id == *id))
            .or(self.all.first().map(|p| p.id));
        self.entries = match self.selected {
            Some(id) => library.playlist_entries(id)?,
            None => Vec::new(),
        };
        Ok(())
    }

    /// Shows the tab. Returns a status message when an action finished or failed.
    pub fn show(&mut self, ui: &mut Ui, library: &mut Library) -> Option<String> {
        let mut message = None;
        egui::Panel::left("playlist_list")
            .default_size(LIST_WIDTH)
            .show(ui, |ui| message = self.sidebar(ui, library));
        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(result) = self.detail(ui, library) {
                message = Some(result);
            }
        });
        message
    }

    fn sidebar(&mut self, ui: &mut Ui, library: &mut Library) -> Option<String> {
        let mut result = None;
        let label = ui.label("New playlist");
        ui.add(egui::TextEdit::singleline(&mut self.new_name).desired_width(LIST_WIDTH - 16.0))
            .labelled_by(label.id);
        ui.horizontal(|ui| {
            let name = self.new_name.trim().to_owned();
            if ui
                .add_enabled(!name.is_empty(), egui::Button::new("Create"))
                .clicked()
            {
                match library.create_playlist(&name) {
                    Ok(id) => {
                        self.selected = Some(id);
                        self.new_name.clear();
                        result = self.after(library, None);
                    }
                    Err(err) => result = Some(err.to_string()),
                }
            }
            if ui.button("Import…").clicked()
                && let Some(file) = pick_import()
            {
                result = self.import(library, file);
            }
        });
        ui.separator();

        let mut clicked = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            for playlist in &self.all {
                let text = format!("{} ({})", playlist.name, playlist.entries);
                if ui
                    .selectable_label(self.selected == Some(playlist.id), text)
                    .clicked()
                {
                    clicked = Some(playlist.id);
                }
            }
        });
        if clicked.is_some() {
            self.selected = clicked;
            return self.after(library, None).or(result);
        }
        result
    }

    fn detail(&mut self, ui: &mut Ui, library: &mut Library) -> Option<String> {
        let Some(id) = self.selected else {
            ui.centered_and_justified(|ui| ui.label("No playlists yet"));
            return None;
        };
        let mut result = None;
        ui.horizontal(|ui| {
            let name = self.all.iter().find(|p| p.id == id).map(|p| p.name.clone());
            ui.heading(name.unwrap_or_default());
            if ui.button("Export…").clicked()
                && let Some(file) = pick_export()
            {
                result = Some(match library.export_playlist(id, &file) {
                    Ok(()) => format!("Exported to {}", file.display()),
                    Err(err) => format!("Export failed: {err}"),
                });
            }
            if ui.button("Delete").clicked() {
                let message = library.delete_playlist(id).err().map(|e| e.to_string());
                result = self.after(library, message);
            }
        });
        ui.separator();

        let mut edit = None;
        egui::ScrollArea::vertical().auto_shrink(false).show_rows(
            ui,
            ROW_HEIGHT,
            self.entries.len(),
            |ui, rows| {
                for i in rows {
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(i > 0, egui::Button::new("Up").small())
                            .clicked()
                        {
                            edit = Some(Edit::Move(i, i - 1));
                        }
                        let can_go_down = i + 1 < self.entries.len();
                        if ui
                            .add_enabled(can_go_down, egui::Button::new("Down").small())
                            .clicked()
                        {
                            edit = Some(Edit::Move(i, i + 1));
                        }
                        if ui.small_button("Remove").clicked() {
                            edit = Some(Edit::Remove(i));
                        }
                        ui.label(entry_text(&self.entries[i]));
                    });
                }
            },
        );
        if let Some(edit) = edit {
            match edit {
                Edit::Move(from, to) => self.entries.swap(from, to),
                Edit::Remove(i) => {
                    self.entries.remove(i);
                }
            }
            let message = library
                .set_playlist_entries(id, &self.entries)
                .err()
                .map(|e| e.to_string());
            result = self.after(library, message);
        }
        result
    }

    fn import(&mut self, library: &mut Library, file: PathBuf) -> Option<String> {
        match library.import_playlist(&file) {
            Ok(report) => {
                self.selected = Some(report.playlist_id);
                let message = format!(
                    "Imported {} tracks, {} not in your library",
                    report.resolved, report.unresolved
                );
                self.after(library, Some(message))
            }
            Err(err) => Some(format!("Import failed: {err}")),
        }
    }

    /// Reloads after a change, keeping `message` unless reloading fails.
    fn after(&mut self, library: &Library, message: Option<String>) -> Option<String> {
        match self.refresh(library) {
            Ok(()) => message,
            Err(err) => Some(err.to_string()),
        }
    }
}

fn entry_text(entry: &Entry) -> RichText {
    let title = entry.title.as_deref().unwrap_or("Unknown");
    let text = match &entry.artist {
        Some(artist) => format!("{artist} - {title}"),
        None => title.to_owned(),
    };
    if entry.track_id.is_some() {
        RichText::new(text)
    } else {
        RichText::new(format!("{text} (not in library)")).weak()
    }
}

fn pick_import() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .add_filter("Playlists", &["m3u8", "m3u", "json"])
        .pick_file()
}

fn pick_export() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .add_filter("M3U8 playlist", &["m3u8"])
        .add_filter("Cantus playlist (keeps MusicBrainz ids)", &["json"])
        .save_file()
}
