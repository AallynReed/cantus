use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

use cantus_core::{Library, ScanReport, Track};
use eframe::egui;

use crate::table::{self, Sort, SortKey};

type ScanResult = cantus_core::Result<ScanReport>;

pub struct App {
    db_path: PathBuf,
    library: Library,
    tracks: Vec<Track>,
    sort: Sort,
    scan: Option<Receiver<ScanResult>>,
    status: String,
}

impl App {
    pub fn new(db_path: PathBuf, library: Library) -> Self {
        let mut app = Self {
            db_path,
            library,
            tracks: Vec::new(),
            sort: Sort {
                key: SortKey::Artist,
                ascending: true,
            },
            scan: None,
            status: String::new(),
        };
        app.reload();
        app
    }

    fn reload(&mut self) {
        match self.library.tracks() {
            Ok(tracks) => {
                self.tracks = tracks;
                self.sort.apply(&mut self.tracks);
            }
            Err(err) => self.status = format!("Cannot load library: {err}"),
        }
    }

    /// Scans `root` in the background, or every known root when `None`.
    fn start_scan(&mut self, ctx: &egui::Context, root: Option<PathBuf>) {
        let (tx, rx) = mpsc::channel();
        let db_path = self.db_path.clone();
        let ctx = ctx.clone();
        thread::spawn(move || {
            // The receiver is gone only if the app closed; nothing left to report to.
            let _ = tx.send(scan(&db_path, root));
            ctx.request_repaint();
        });
        self.scan = Some(rx);
        self.status = "Scanning…".into();
    }

    fn poll_scan(&mut self) {
        let Some(rx) = &self.scan else { return };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                self.scan = None;
                self.status = "Scan stopped unexpectedly".into();
                return;
            }
        };
        self.scan = None;
        self.status = match result {
            Ok(r) => summary(&r),
            Err(err) => format!("Scan failed: {err}"),
        };
        self.reload();
    }
}

fn scan(db_path: &Path, root: Option<PathBuf>) -> ScanResult {
    let mut library = Library::open(db_path)?;
    let roots = match root {
        Some(root) => vec![root],
        None => library.roots()?,
    };
    let mut report = ScanReport::default();
    for root in roots {
        report.merge(library.scan(&root)?);
    }
    Ok(report)
}

fn summary(r: &ScanReport) -> String {
    let mut s = format!(
        "{} added, {} updated, {} moved, {} missing, {} unchanged",
        r.added, r.updated, r.moved, r.missing, r.unchanged
    );
    if r.unreadable > 0 {
        s += &format!(", {} unreadable", r.unreadable);
    }
    if !r.skipped.is_empty() {
        s += &format!(", {} skipped", r.skipped.len());
    }
    s
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _: &mut eframe::Frame) {
        self.poll_scan();
        let scanning = self.scan.is_some();

        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                let add = ui.add_enabled(!scanning, egui::Button::new("Add folder…"));
                if add.clicked()
                    && let Some(dir) = rfd::FileDialog::new().pick_folder()
                {
                    self.start_scan(ui.ctx(), Some(dir));
                }
                if ui
                    .add_enabled(!scanning, egui::Button::new("Rescan"))
                    .clicked()
                {
                    self.start_scan(ui.ctx(), None);
                }
                if scanning {
                    ui.spinner();
                }
                ui.label(&self.status);
            });
        });

        egui::Panel::bottom("status").show(ui, |ui| {
            let n = self.tracks.len();
            ui.label(format!("{n} track{}", if n == 1 { "" } else { "s" }));
        });

        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(key) = table::show(ui, &self.tracks, self.sort) {
                self.sort.toggle(key);
                self.sort.apply(&mut self.tracks);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::{Harness, kittest::Queryable};

    #[test]
    fn starts_with_empty_library() {
        let library = Library::open_in_memory().unwrap();
        let harness = Harness::new_eframe(|_| App::new(PathBuf::new(), library));
        harness.get_by_label("Add folder…");
        harness.get_by_label("Library empty");
        harness.get_by_label("0 tracks");
    }

    #[test]
    fn background_scan_results_show_after_reload() {
        let dir = tempfile::tempdir().unwrap();
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../cantus-core/tests/fixtures/tone.flac");
        std::fs::copy(fixture, dir.path().join("tone.flac")).unwrap();
        let db_path = dir.path().join("library.db");

        let report = scan(&db_path, Some(dir.path().to_path_buf())).unwrap();
        assert_eq!(
            summary(&report),
            "1 added, 0 updated, 0 moved, 0 missing, 0 unchanged"
        );

        let library = Library::open(&db_path).unwrap();
        let harness = Harness::new_eframe(|_| App::new(db_path.clone(), library));
        harness.get_by_label("Test Tone");
        harness.get_by_label("1 track");
    }
}
