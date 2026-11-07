use cantus_core::{Track, TrackState};
use eframe::egui::{self, RichText, Ui};
use egui_extras::{Column, TableBuilder};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Title,
    Artist,
    Album,
    Year,
    Length,
}

const COLUMNS: [(SortKey, &str); 5] = [
    (SortKey::Title, "Title"),
    (SortKey::Artist, "Artist"),
    (SortKey::Album, "Album"),
    (SortKey::Year, "Year"),
    (SortKey::Length, "Length"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sort {
    pub key: SortKey,
    pub ascending: bool,
}

impl Sort {
    /// Clicking the active column flips direction; another column sorts ascending.
    pub fn toggle(&mut self, key: SortKey) {
        self.ascending = self.key != key || !self.ascending;
        self.key = key;
    }

    pub fn apply(self, tracks: &mut [Track]) {
        tracks.sort_by_cached_key(|t| sort_key(t, self.key));
        if !self.ascending {
            tracks.reverse();
        }
    }
}

fn sort_key(t: &Track, key: SortKey) -> (String, u64) {
    let m = &t.meta;
    let text = |s: &Option<String>| s.as_deref().unwrap_or_default().to_lowercase();
    match key {
        SortKey::Title => (title(t).to_lowercase(), 0),
        SortKey::Artist => (text(&m.artist), 0),
        SortKey::Album => {
            let pos = u64::from(m.disc_no.unwrap_or(0)) << 32 | u64::from(m.track_no.unwrap_or(0));
            (text(&m.album), pos)
        }
        SortKey::Year => (String::new(), m.year.map_or(0, u64::from)),
        SortKey::Length => (String::new(), m.duration_ms.map_or(0, u64::from)),
    }
}

/// Tag title, or the file name when the file has none.
fn title(t: &Track) -> String {
    t.meta.title.clone().unwrap_or_else(|| {
        t.path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    })
}

fn length(ms: Option<u32>) -> String {
    ms.map(|ms| format!("{}:{:02}", ms / 60_000, ms / 1000 % 60))
        .unwrap_or_default()
}

/// Shows the track table and returns the column whose header was clicked.
pub fn show(ui: &mut Ui, tracks: &[Track], sort: Sort) -> Option<SortKey> {
    if tracks.is_empty() {
        ui.centered_and_justified(|ui| ui.label("Library empty"));
        return None;
    }

    let mut clicked = None;
    TableBuilder::new(ui)
        .striped(true)
        .resizable(true)
        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
        .columns(Column::remainder().at_least(80.0).clip(true), 3)
        .columns(Column::auto().at_least(50.0), 2)
        .header(22.0, |mut header| {
            for (key, label) in COLUMNS {
                header.col(|ui| {
                    if ui.button(label).clicked() {
                        clicked = Some(key);
                    }
                    if sort.key == key {
                        sort_arrow(ui, sort.ascending);
                    }
                });
            }
        })
        .body(|body| {
            body.rows(20.0, tracks.len(), |mut row| {
                let t = &tracks[row.index()];
                let m = &t.meta;
                let hover = match t.state {
                    TrackState::Ok => t.path.display().to_string(),
                    TrackState::Missing => format!("Missing: {}", t.path.display()),
                    TrackState::Unreadable => {
                        format!(
                            "{}: {}",
                            t.path.display(),
                            t.error.as_deref().unwrap_or("unreadable")
                        )
                    }
                };
                let cells = [
                    title(t),
                    m.artist.clone().unwrap_or_default(),
                    m.album.clone().unwrap_or_default(),
                    m.year.map(|y| y.to_string()).unwrap_or_default(),
                    length(m.duration_ms),
                ];
                for (i, text) in cells.into_iter().enumerate() {
                    row.col(|ui| {
                        let text = RichText::new(text);
                        let text = if t.state == TrackState::Ok {
                            text
                        } else {
                            text.weak()
                        };
                        let label = ui.label(text);
                        if i == 0 {
                            label.on_hover_text(&hover);
                        }
                    });
                }
            });
        });
    clicked
}

/// Painted rather than a glyph: egui's bundled font has no arrows.
fn sort_arrow(ui: &mut Ui, ascending: bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 6.0), egui::Sense::hover());
    let (tip, base) = if ascending {
        (rect.top(), rect.bottom())
    } else {
        (rect.bottom(), rect.top())
    };
    let points = vec![
        egui::pos2(rect.center().x, tip),
        egui::pos2(rect.left(), base),
        egui::pos2(rect.right(), base),
    ];
    let color = ui.visuals().text_color();
    ui.painter().add(egui::Shape::convex_polygon(
        points,
        color,
        egui::Stroke::NONE,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use cantus_core::Metadata;
    use egui_kittest::{Harness, kittest::Queryable};

    fn track(id: i64, title: &str, album: &str, track_no: u32) -> Track {
        Track {
            id,
            path: format!("{title}.flac").into(),
            state: TrackState::Ok,
            error: None,
            meta: Metadata {
                title: Some(title.into()),
                album: Some(album.into()),
                track_no: Some(track_no),
                duration_ms: Some(83_000),
                ..Default::default()
            },
        }
    }

    fn ids(tracks: &[Track]) -> Vec<i64> {
        tracks.iter().map(|t| t.id).collect()
    }

    #[test]
    fn album_sort_orders_by_track_number_and_flips() {
        let mut tracks = vec![
            track(1, "b", "X", 2),
            track(2, "a", "X", 1),
            track(3, "c", "A", 9),
        ];
        let mut sort = Sort {
            key: SortKey::Title,
            ascending: true,
        };

        sort.toggle(SortKey::Album);
        sort.apply(&mut tracks);
        assert_eq!(ids(&tracks), [3, 2, 1]);

        sort.toggle(SortKey::Album);
        sort.apply(&mut tracks);
        assert_eq!(ids(&tracks), [1, 2, 3]);
    }

    #[test]
    fn title_falls_back_to_file_name() {
        let mut t = track(1, "song", "", 1);
        t.meta.title = None;
        assert_eq!(title(&t), "song.flac");
    }

    #[test]
    fn shows_rows_and_empty_state() {
        let sort = Sort {
            key: SortKey::Title,
            ascending: true,
        };
        let tracks = [track(1, "First Song", "Album", 1)];
        let harness = Harness::new_ui(|ui| {
            show(ui, &tracks, sort);
        });
        harness.get_by_label("First Song");
        harness.get_by_label("1:23");

        let harness = Harness::new_ui(|ui| {
            show(ui, &[], sort);
        });
        harness.get_by_label("Library empty");
    }
}
