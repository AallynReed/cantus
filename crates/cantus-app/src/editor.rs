use std::collections::BTreeSet;

use cantus_core::{Field, TagChange, Track};
use eframe::egui::{self, Ui};

/// Tag editor for the selected tracks. Only fields the user changes are written.
pub struct Editor {
    ids: BTreeSet<i64>,
    fields: Vec<FieldEdit>,
}

pub enum Action {
    None,
    Apply,
    Identify,
    Preview,
    Stop,
}

struct FieldEdit {
    field: Field,
    /// The value shared by every selected track, or `None` when they differ.
    shared: Option<String>,
    text: String,
}

impl FieldEdit {
    fn change(&self) -> Option<TagChange> {
        let unchanged = match &self.shared {
            Some(shared) => self.text == *shared,
            None => self.text.is_empty(),
        };
        (!unchanged).then(|| TagChange {
            field: self.field,
            value: Some(self.text.clone()),
        })
    }
}

impl Editor {
    pub fn new(tracks: &[Track], ids: &BTreeSet<i64>) -> Self {
        let selected: Vec<_> = tracks.iter().filter(|t| ids.contains(&t.id)).collect();
        let fields = Field::ALL
            .into_iter()
            .map(|field| {
                let mut values = selected
                    .iter()
                    .map(|t| field.get(&t.meta).unwrap_or_default());
                let first = values.next().unwrap_or_default();
                let shared = values.all(|v| v == first).then_some(first);
                FieldEdit {
                    field,
                    text: shared.clone().unwrap_or_default(),
                    shared,
                }
            })
            .collect();
        Self {
            ids: ids.clone(),
            fields,
        }
    }

    pub fn ids(&self) -> &BTreeSet<i64> {
        &self.ids
    }

    pub fn changes(&self) -> Vec<TagChange> {
        self.fields.iter().filter_map(FieldEdit::change).collect()
    }

    pub fn show(&mut self, ui: &mut Ui, busy: bool, playing: bool) -> Action {
        let n = self.ids.len();
        ui.heading(format!(
            "{n} track{} selected",
            if n == 1 { "" } else { "s" }
        ));
        ui.add_space(8.0);

        let mut valid = true;
        egui::Grid::new("tag_fields")
            .num_columns(2)
            .spacing([8.0, 6.0])
            .show(ui, |ui| {
                for edit in &mut self.fields {
                    let label = ui.label(edit.field.label());
                    let hint = if edit.shared.is_none() {
                        "(multiple values)"
                    } else {
                        ""
                    };
                    ui.add(
                        egui::TextEdit::singleline(&mut edit.text)
                            .hint_text(hint)
                            .desired_width(220.0),
                    )
                    .labelled_by(label.id);
                    ui.end_row();
                    if let Some(Err(err)) = edit.change().map(|c| c.normalized()) {
                        valid = false;
                        ui.label("");
                        ui.colored_label(ui.visuals().error_fg_color, err);
                        ui.end_row();
                    }
                }
            });

        ui.add_space(8.0);
        let changed = !self.changes().is_empty();
        let mut action = Action::None;
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!busy && changed && valid, egui::Button::new("Apply"))
                .clicked()
            {
                action = Action::Apply;
            }
            if ui
                .add_enabled(!busy, egui::Button::new("Identify"))
                .clicked()
            {
                action = Action::Identify;
            }
            let text = if playing { "Stop" } else { "Preview" };
            if ui.add_enabled(n == 1, egui::Button::new(text)).clicked() {
                action = if playing {
                    Action::Stop
                } else {
                    Action::Preview
                };
            }
        });
        action
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cantus_core::{Metadata, TrackState};

    fn track(id: i64, artist: &str, year: u16) -> Track {
        Track {
            id,
            path: format!("{id}.flac").into(),
            state: TrackState::Ok,
            error: None,
            meta: Metadata {
                artist: Some(artist.into()),
                year: Some(year),
                ..Default::default()
            },
        }
    }

    #[test]
    fn only_edited_fields_become_changes() {
        let tracks = [track(1, "A", 2000), track(2, "A", 2001)];
        let mut editor = Editor::new(&tracks, &BTreeSet::from([1, 2]));
        assert!(editor.changes().is_empty());

        let year = editor
            .fields
            .iter_mut()
            .find(|f| f.field == Field::Year)
            .unwrap();
        assert_eq!(year.shared, None, "years differ");
        year.text = "1999".into();
        let artist = editor
            .fields
            .iter_mut()
            .find(|f| f.field == Field::Artist)
            .unwrap();
        assert_eq!(artist.shared.as_deref(), Some("A"));
        artist.text = String::new();

        assert_eq!(
            editor.changes(),
            [
                TagChange {
                    field: Field::Artist,
                    value: Some(String::new())
                },
                TagChange {
                    field: Field::Year,
                    value: Some("1999".into())
                },
            ]
        );
    }
}
