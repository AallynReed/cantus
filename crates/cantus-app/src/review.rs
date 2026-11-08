use std::collections::HashSet;

use cantus_core::{Candidate, Field, FieldDiff, Identified, TagChange, Track};
use eframe::egui::{self, Ui};

const LIST_WIDTH: f32 = 260.0;

/// The review screen for identification results. Nothing is written until Apply.
pub struct Review {
    items: Vec<Item>,
    current: usize,
    pub embed_cover: bool,
}

struct Item {
    track: Track,
    candidates: Vec<Candidate>,
    error: Option<String>,
    chosen: usize,
    include: bool,
    rejected: HashSet<Field>,
}

impl Item {
    fn candidate(&self) -> Option<&Candidate> {
        self.candidates.get(self.chosen)
    }

    fn diffs(&self) -> Vec<FieldDiff> {
        self.candidate()
            .map(|c| c.diff(&self.track.meta))
            .unwrap_or_default()
    }

    fn accepted(&self) -> Option<Accepted> {
        let candidate = self.candidate().filter(|_| self.include)?;
        let changes: Vec<TagChange> = self
            .diffs()
            .into_iter()
            .filter(|d| !self.rejected.contains(&d.field))
            .map(|d| TagChange {
                field: d.field,
                value: Some(d.new),
            })
            .collect();
        (!changes.is_empty()).then(|| Accepted {
            track_id: self.track.id,
            changes,
            release: candidate.release_mbid.clone(),
        })
    }
}

/// One track's approved changes.
pub struct Accepted {
    pub track_id: i64,
    pub changes: Vec<TagChange>,
    pub release: String,
}

impl Review {
    pub fn new(results: Vec<Identified>, tracks: &[Track]) -> Self {
        let items = results
            .into_iter()
            .filter_map(|result| {
                let track = tracks.iter().find(|t| t.id == result.track_id)?.clone();
                let include = !result.candidates.is_empty();
                Some(Item {
                    track,
                    candidates: result.candidates,
                    error: result.error,
                    chosen: 0,
                    include,
                    rejected: HashSet::new(),
                })
            })
            .collect();
        Self {
            items,
            current: 0,
            embed_cover: true,
        }
    }

    fn accepted(&self) -> Vec<Accepted> {
        self.items.iter().filter_map(Item::accepted).collect()
    }

    /// Returns the approved changes when the user clicks Apply.
    pub fn show(&mut self, ui: &mut Ui, busy: bool) -> Option<Vec<Accepted>> {
        let accepted = self.accepted();
        let mut apply = false;
        egui::Panel::top("review_actions").show(ui, |ui| {
            ui.horizontal(|ui| {
                let n = accepted.len();
                let text = format!("Apply to {n} track{}", if n == 1 { "" } else { "s" });
                apply = ui
                    .add_enabled(!busy && n > 0, egui::Button::new(text))
                    .clicked();
                ui.checkbox(&mut self.embed_cover, "Embed cover art");
            });
        });
        egui::Panel::left("review_list")
            .default_size(LIST_WIDTH)
            .show(ui, |ui| self.track_list(ui));
        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(item) = self.items.get_mut(self.current) {
                show_item(ui, item);
            }
        });
        apply.then_some(accepted)
    }

    fn track_list(&mut self, ui: &mut Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            for (i, item) in self.items.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(!item.candidates.is_empty(), |ui| {
                        ui.checkbox(&mut item.include, "")
                            .on_hover_text("Include this track");
                    });
                    let status = match item.candidates.len() {
                        0 => "no match".to_owned(),
                        _ => format!("{}%", item.candidates[item.chosen].score),
                    };
                    let name = item
                        .track
                        .path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy();
                    let text = format!("{name}  ({status})");
                    if ui.selectable_label(i == self.current, text).clicked() {
                        self.current = i;
                    }
                });
            }
        });
    }
}

fn describe(c: &Candidate) -> String {
    let year = c.year.map(|y| format!(" ({y})")).unwrap_or_default();
    let source = if c.by_fingerprint { "audio" } else { "tags" };
    format!(
        "{} - {} - {}{year}  [{}% by {source}]",
        c.artist, c.title, c.album, c.score
    )
}

fn show_item(ui: &mut Ui, item: &mut Item) {
    ui.heading(item.track.path.display().to_string());
    if item.candidates.is_empty() {
        let reason = item
            .error
            .as_deref()
            .unwrap_or("MusicBrainz has no match for this track.");
        ui.label(reason);
        return;
    }

    let label = ui.label("Match");
    let selected = item.candidate().map(describe).unwrap_or_default();
    let combo = egui::ComboBox::from_id_salt("candidate")
        .width(ui.available_width().min(560.0))
        .selected_text(selected)
        .show_ui(ui, |ui| {
            for (i, candidate) in item.candidates.iter().enumerate() {
                ui.selectable_value(&mut item.chosen, i, describe(candidate));
            }
        });
    combo.response.labelled_by(label.id);
    ui.add_space(8.0);

    let diffs = item.diffs();
    if diffs.is_empty() {
        ui.label("The tags already match.");
        return;
    }
    egui::Grid::new("diff")
        .striped(true)
        .num_columns(3)
        .show(ui, |ui| {
            ui.strong("Field");
            ui.strong("Current");
            ui.strong("New");
            ui.end_row();
            for diff in diffs {
                let mut accept = !item.rejected.contains(&diff.field);
                if ui.checkbox(&mut accept, diff.field.label()).changed() {
                    if accept {
                        item.rejected.remove(&diff.field);
                    } else {
                        item.rejected.insert(diff.field);
                    }
                }
                ui.label(diff.old.unwrap_or_default());
                ui.label(diff.new);
                ui.end_row();
            }
        });
}
