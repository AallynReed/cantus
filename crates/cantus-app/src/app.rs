use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use cantus_core::{BatchReport, ChangeSetSummary, Library, ScanReport, Track};
use eframe::egui;

use crate::editor::Editor;
use crate::table::{self, Selection, Sort, SortKey};

/// Background work runs on its own thread and database connection; it reports a status line.
type JobResult = Result<String, String>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Library,
    History,
}

pub struct App {
    db_path: PathBuf,
    library: Library,
    tracks: Vec<Track>,
    history: Vec<ChangeSetSummary>,
    sort: Sort,
    selection: Selection,
    editor: Option<Editor>,
    view: View,
    job: Option<Receiver<JobResult>>,
    status: String,
}

impl App {
    pub fn new(db_path: PathBuf, mut library: Library) -> Self {
        let status = match library.recover() {
            Ok(0) => String::new(),
            Ok(n) => format!("Settled {n} changes interrupted by a crash; see History"),
            Err(err) => format!("Cannot check for interrupted changes: {err}"),
        };
        let mut app = Self {
            db_path,
            library,
            tracks: Vec::new(),
            history: Vec::new(),
            sort: Sort {
                key: SortKey::Artist,
                ascending: true,
            },
            selection: Selection::default(),
            editor: None,
            view: View::Library,
            job: None,
            status,
        };
        app.reload();
        app
    }

    fn reload(&mut self) {
        let loaded = self
            .library
            .tracks()
            .and_then(|t| Ok((t, self.library.history()?)));
        match loaded {
            Ok((tracks, history)) => {
                self.tracks = tracks;
                self.history = history;
                self.sort.apply(&mut self.tracks);
                self.selection.anchor = None;
                self.editor = None;
            }
            Err(err) => self.status = format!("Cannot load library: {err}"),
        }
    }

    fn run_job(
        &mut self,
        ctx: &egui::Context,
        label: &str,
        work: impl FnOnce(&mut Library) -> cantus_core::Result<String> + Send + 'static,
    ) {
        let (tx, rx) = mpsc::channel();
        let db_path = self.db_path.clone();
        let ctx = ctx.clone();
        let failed = format!("{label} failed");
        thread::spawn(move || {
            let result = Library::open(&db_path)
                .and_then(|mut library| work(&mut library))
                .map_err(|err| format!("{failed}: {err}"));
            // The receiver is gone only if the app closed; nothing left to report to.
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        self.job = Some(rx);
        self.status = format!("{label}…");
    }

    fn poll_job(&mut self) {
        let Some(rx) = &self.job else { return };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err("Background task stopped unexpectedly".into()),
        };
        self.job = None;
        self.status = result.unwrap_or_else(|err| err);
        self.reload();
    }

    fn scan(&mut self, ctx: &egui::Context, root: Option<PathBuf>) {
        self.run_job(ctx, "Scanning", move |library| {
            Ok(summary(&scan(library, root)?))
        });
    }

    fn apply_edits(&mut self, ctx: &egui::Context) {
        let Some(editor) = &self.editor else { return };
        let ids: Vec<_> = editor.ids().iter().copied().collect();
        let changes = editor.changes();
        self.run_job(ctx, "Saving tags", move |library| {
            let (_, report) = library.edit_tags(&ids, &changes)?;
            Ok(batch_summary("Saved", &report))
        });
    }

    fn undo(&mut self, ctx: &egui::Context, change_set: i64) {
        self.run_job(ctx, "Undoing", move |library| {
            Ok(batch_summary("Restored", &library.undo(change_set)?))
        });
    }

    fn library_view(&mut self, ui: &mut egui::Ui, busy: bool) {
        if self.editor.as_ref().map(Editor::ids) != Some(&self.selection.ids) {
            self.editor = (!self.selection.ids.is_empty())
                .then(|| Editor::new(&self.tracks, &self.selection.ids));
        }
        let mut apply = false;
        if let Some(editor) = &mut self.editor {
            egui::Panel::right("editor").show(ui, |ui| apply = editor.show(ui, busy));
        }
        if apply {
            self.apply_edits(ui.ctx());
        }

        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(key) = table::show(ui, &self.tracks, self.sort, &mut self.selection) {
                self.sort.toggle(key);
                self.sort.apply(&mut self.tracks);
                self.selection.anchor = None;
            }
        });
    }

    fn history_view(&mut self, ui: &mut egui::Ui, busy: bool) {
        let mut undo = None;
        egui::CentralPanel::default().show(ui, |ui| {
            if self.history.is_empty() {
                ui.centered_and_justified(|ui| ui.label("No changes yet"));
                return;
            }
            let now = unix_now();
            egui::ScrollArea::vertical().show(ui, |ui| {
                egui::Grid::new("history")
                    .striped(true)
                    .num_columns(4)
                    .show(ui, |ui| {
                        for c in &self.history {
                            ui.label(ago(now - c.created_at));
                            ui.label(&c.description);
                            ui.label(counts(c));
                            let can_undo = !busy && c.done > 0;
                            if ui
                                .add_enabled(can_undo, egui::Button::new("Undo"))
                                .clicked()
                            {
                                undo = Some(c.id);
                            }
                            ui.end_row();
                        }
                    });
            });
        });
        if let Some(id) = undo {
            self.undo(ui.ctx(), id);
        }
    }
}

fn scan(library: &mut Library, root: Option<PathBuf>) -> cantus_core::Result<ScanReport> {
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

fn batch_summary(verb: &str, r: &BatchReport) -> String {
    let n = r.done;
    let mut s = format!("{verb} {n} file{}", if n == 1 { "" } else { "s" });
    if let Some((path, reason)) = r.failed.first() {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        s += &format!("; {} failed, e.g. {name}: {reason}", r.failed.len());
    }
    s
}

fn counts(c: &ChangeSetSummary) -> String {
    let mut s = format!("{} applied", c.done);
    for (n, what) in [(c.undone, "undone"), (c.failed, "failed")] {
        if n > 0 {
            s += &format!(", {n} {what}");
        }
    }
    s
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().try_into().unwrap_or(i64::MAX))
}

fn ago(secs: i64) -> String {
    match secs {
        ..60 => "just now".into(),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86_400 => format!("{} h ago", secs / 3600),
        _ => format!("{} days ago", secs / 86_400),
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _: &mut eframe::Frame) {
        self.poll_job();
        let busy = self.job.is_some();

        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.view, View::Library, "Library");
                ui.selectable_value(&mut self.view, View::History, "History");
                ui.separator();
                let add = ui.add_enabled(!busy, egui::Button::new("Add folder…"));
                if add.clicked()
                    && let Some(dir) = rfd::FileDialog::new().pick_folder()
                {
                    self.scan(ui.ctx(), Some(dir));
                }
                if ui.add_enabled(!busy, egui::Button::new("Rescan")).clicked() {
                    self.scan(ui.ctx(), None);
                }
                if busy {
                    ui.spinner();
                }
                ui.label(&self.status);
            });
        });

        egui::Panel::bottom("status").show(ui, |ui| {
            let n = self.tracks.len();
            ui.label(format!("{n} track{}", if n == 1 { "" } else { "s" }));
        });

        match self.view {
            View::Library => self.library_view(ui, busy),
            View::History => self.history_view(ui, busy),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::{Harness, kittest::Queryable};
    use std::path::Path;

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

        let mut library = Library::open(&db_path).unwrap();
        let report = scan(&mut library, Some(dir.path().to_path_buf())).unwrap();
        assert_eq!(
            summary(&report),
            "1 added, 0 updated, 0 moved, 0 missing, 0 unchanged"
        );

        let mut harness = Harness::new_eframe(|_| App::new(db_path.clone(), library));
        harness.get_by_label("1 track");

        harness.get_by_label("Test Tone").click();
        harness.run();
        harness.get_by_label("1 track selected");

        harness.get_by_label("History").click();
        harness.run();
        harness.get_by_label("No changes yet");
    }
}
