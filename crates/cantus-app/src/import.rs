use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

use cantus_core::{InboxBatch, PlannedMove, TorrentFile, TorrentId, TorrentState, TorrentStatus};
use eframe::egui::{self, Ui};

use crate::platform;

/// The Import tab: add links, watch downloads, and bring finished ones into the library.
#[derive(Default)]
pub struct Import {
    input: String,
    pub statuses: Vec<TorrentStatus>,
    pub files: HashMap<TorrentId, Vec<TorrentFile>>,
    pub batches: Vec<InboxBatch>,
    pub plan: Option<(i64, Vec<PlannedMove>)>,
    pub hardlink: bool,
}

pub enum Action {
    None,
    Add(String),
    AddFile(PathBuf),
    Resume(TorrentId),
    Pause(TorrentId),
    Remove(TorrentId),
    Select(TorrentId, BTreeSet<usize>),
    Plan(i64),
    Apply(i64, Vec<PlannedMove>),
    Dismiss(i64),
}

fn megabytes(bytes: f64) -> String {
    format!("{:.1} MB", bytes / 1_000_000.0)
}

impl Import {
    pub fn show(&mut self, ui: &mut Ui, torrents_on: bool, busy: bool) -> Action {
        let mut action = Action::None;
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                self.add_row(ui, torrents_on, busy, &mut action);
                ui.add_space(12.0);
                self.downloads(ui, &mut action);
                ui.add_space(12.0);
                self.inbox(ui, busy, &mut action);
            });
        action
    }

    fn add_row(&mut self, ui: &mut Ui, torrents_on: bool, busy: bool, action: &mut Action) {
        ui.heading("Add");
        let label = ui.label("Magnet link, or a web link to your own audio");
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.input).desired_width(420.0))
                .labelled_by(label.id);
            let link = self.input.trim().to_owned();
            if ui
                .add_enabled(!busy && !link.is_empty(), egui::Button::new("Add"))
                .clicked()
            {
                *action = Action::Add(link);
                self.input.clear();
            }
            if ui
                .add_enabled(torrents_on, egui::Button::new("Open .torrent…"))
                .clicked()
                && let Some(file) = platform::pick_torrent()
            {
                *action = Action::AddFile(file);
            }
        });
        ui.small(
            "Torrent peers can see your IP address; a proxy can be set in Settings. You are \
             responsible for what you download and for following each site's terms.",
        );
    }

    fn downloads(&mut self, ui: &mut Ui, action: &mut Action) {
        ui.heading("Downloads");
        if self.statuses.is_empty() {
            ui.label("No torrents");
        }
        for status in &self.statuses {
            ui.horizontal(|ui| {
                let fraction = status.done_bytes as f32 / status.total_bytes.max(1) as f32;
                ui.add(
                    egui::ProgressBar::new(fraction)
                        .desired_width(120.0)
                        .show_percentage(),
                );
                ui.label(&status.name);
                ui.weak(describe(status));
                match status.state {
                    TorrentState::Paused => {
                        if ui.small_button("Start").clicked() {
                            *action = Action::Resume(status.id);
                        }
                    }
                    TorrentState::Downloading | TorrentState::Seeding => {
                        if ui.small_button("Pause").clicked() {
                            *action = Action::Pause(status.id);
                        }
                    }
                    TorrentState::Checking | TorrentState::Failed => {}
                }
                if ui.small_button("Remove").clicked() {
                    *action = Action::Remove(status.id);
                }
            });
            if status.state == TorrentState::Paused && !status.finished {
                file_picker(ui, status.id, self.files.get(&status.id), action);
            }
        }
    }

    fn inbox(&mut self, ui: &mut Ui, busy: bool, action: &mut Action) {
        ui.heading("Inbox");
        let waiting: Vec<&InboxBatch> = self
            .batches
            .iter()
            .filter(|b| b.imported_by.is_none())
            .collect();
        if waiting.is_empty() {
            ui.label("Nothing waiting to be imported");
            return;
        }
        ui.checkbox(
            &mut self.hardlink,
            "Hardlink instead of copying when on the same drive",
        );
        for batch in waiting {
            ui.horizontal(|ui| {
                ui.label(format!(
                    "{} ({} files, {})",
                    batch.name,
                    batch.files.len(),
                    batch.origin
                ));
                if ui
                    .add_enabled(!busy, egui::Button::new("Preview import").small())
                    .clicked()
                {
                    *action = Action::Plan(batch.id);
                }
                if ui.small_button("Dismiss").clicked() {
                    *action = Action::Dismiss(batch.id);
                }
            });
            if let Some((_, plan)) = self.plan.as_ref().filter(|(id, _)| *id == batch.id) {
                for m in plan {
                    let to = m.to.strip_prefix(&m.root).unwrap_or(&m.to);
                    ui.monospace(to.display().to_string());
                }
                let text = format!("Import {} files", plan.len());
                if ui.add_enabled(!busy, egui::Button::new(text)).clicked() {
                    *action = Action::Apply(batch.id, plan.clone());
                }
            }
        }
    }
}

fn describe(status: &TorrentStatus) -> String {
    match (status.state, &status.error) {
        (TorrentState::Failed, Some(err)) => format!("failed: {err}"),
        (TorrentState::Failed, None) => "failed".to_owned(),
        (TorrentState::Checking, _) => "checking".to_owned(),
        (TorrentState::Paused, _) => "paused".to_owned(),
        (TorrentState::Seeding, _) => {
            format!("seeding, {}/s up", megabytes(status.up_bytes_per_sec))
        }
        (TorrentState::Downloading, _) => format!(
            "{}/s down, {}/s up",
            megabytes(status.down_bytes_per_sec),
            megabytes(status.up_bytes_per_sec)
        ),
    }
}

fn file_picker(ui: &mut Ui, id: TorrentId, files: Option<&Vec<TorrentFile>>, action: &mut Action) {
    let Some(files) = files else { return };
    egui::CollapsingHeader::new(format!("Files ({})", files.len()))
        .id_salt(("files", id.0))
        .show(ui, |ui| {
            for file in files {
                let mut selected = file.selected;
                let text = format!("{} ({})", file.path.display(), megabytes(file.len as f64));
                if ui.checkbox(&mut selected, text).changed() {
                    let chosen = files
                        .iter()
                        .filter(|f| {
                            (f.index == file.index && selected)
                                || (f.index != file.index && f.selected)
                        })
                        .map(|f| f.index)
                        .collect();
                    *action = Action::Select(id, chosen);
                }
            }
        });
}
