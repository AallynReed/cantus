use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cantus_core::{
    BatchReport, ChangeSetSummary, DEFAULT_TEMPLATE, Endpoints, Identified, Library, Lookup, Pcm,
    PlannedMove, ScanReport, Template, Track, TrackEdit, decode_preview,
};
use eframe::egui;

use crate::editor::{self, Editor};
use crate::organize::{self, Organize};
use crate::player::Player;
use crate::playlists::Playlists;
use crate::review::{Accepted, Review};
use crate::settings::{self, Settings};
use crate::table::{self, Selection, Sort, SortKey};

const TEMPLATE_SETTING: &str = "template";
const PREVIEW_SECONDS: u32 = 60;

/// Background work runs on its own thread and database connection.
type JobResult = Result<JobOutput, String>;

struct JobOutput {
    status: String,
    data: JobData,
}

enum JobData {
    None,
    Plan(Vec<PlannedMove>),
    Identified(Vec<Identified>),
}

impl From<String> for JobOutput {
    fn from(status: String) -> Self {
        Self {
            status,
            data: JobData::None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Library,
    Organize,
    Identify,
    Playlists,
    History,
    Settings,
}

pub struct App {
    db_path: PathBuf,
    library: Library,
    tracks: Vec<Track>,
    history: Vec<ChangeSetSummary>,
    sort: Sort,
    selection: Selection,
    editor: Option<Editor>,
    organize: Organize,
    view: View,
    job: Option<Receiver<JobResult>>,
    progress: Arc<Mutex<String>>,
    cancel: Arc<AtomicBool>,
    review: Option<Review>,
    playlists: Playlists,
    settings: Settings,
    player: Player,
    decoding: Option<Receiver<Result<Pcm, String>>>,
    status: String,
}

impl App {
    pub fn new(db_path: PathBuf, mut library: Library) -> Self {
        let status = match library.recover() {
            Ok(0) => String::new(),
            Ok(n) => format!("Settled {n} changes interrupted by a crash; see History"),
            Err(err) => format!("Cannot check for interrupted changes: {err}"),
        };
        let template = library
            .setting(TEMPLATE_SETTING)
            .ok()
            .flatten()
            .unwrap_or_else(|| DEFAULT_TEMPLATE.to_owned());
        let settings = Settings::load(&library);
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
            organize: Organize::new(template),
            view: View::Library,
            job: None,
            progress: Arc::default(),
            cancel: Arc::default(),
            review: None,
            playlists: Playlists::default(),
            settings,
            player: Player::default(),
            decoding: None,
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
                self.organize.plan = None;
                if let Err(err) = self.playlists.refresh(&self.library) {
                    self.status = format!("Cannot load playlists: {err}");
                }
            }
            Err(err) => self.status = format!("Cannot load library: {err}"),
        }
    }

    fn run_job(
        &mut self,
        ctx: &egui::Context,
        label: &str,
        work: impl FnOnce(&mut Library) -> cantus_core::Result<JobOutput> + Send + 'static,
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
        self.progress = Arc::default();
        self.cancel = Arc::default();
        self.reload();
        match result {
            Ok(output) => {
                self.status = output.status;
                match output.data {
                    JobData::None => {}
                    JobData::Plan(plan) => self.organize.plan = Some(plan),
                    JobData::Identified(results) => {
                        self.review = Some(Review::new(results, &self.tracks));
                        self.view = View::Identify;
                    }
                }
            }
            Err(err) => self.status = err,
        }
    }

    fn scan(&mut self, ctx: &egui::Context, root: Option<PathBuf>) {
        self.run_job(ctx, "Scanning", move |library| {
            Ok(summary(&scan(library, root)?).into())
        });
    }

    fn apply_edits(&mut self, ctx: &egui::Context) {
        let Some(editor) = &self.editor else { return };
        let ids: Vec<_> = editor.ids().iter().copied().collect();
        let changes = editor.changes();
        self.run_job(ctx, "Saving tags", move |library| {
            let (_, report) = library.edit_tags(&ids, &changes)?;
            Ok(batch_summary("Saved", &report).into())
        });
    }

    fn undo(&mut self, ctx: &egui::Context, change_set: i64) {
        self.run_job(ctx, "Undoing", move |library| {
            Ok(batch_summary("Restored", &library.undo(change_set)?).into())
        });
    }

    fn identify(&mut self, ctx: &egui::Context) {
        if !self.settings.online_lookups {
            self.status = "Allow online lookups in Settings to identify tracks".into();
            self.view = View::Settings;
            return;
        }
        let ids = self.selection.ids.clone();
        let progress = self.progress_callback(ctx);
        self.run_job(ctx, "Identifying", move |library| {
            let key = settings::acoustid_key(library);
            let mut lookup = Lookup::new(Endpoints::default(), key)?;
            let results = library.identify(&mut lookup, &ids, progress)?;
            let matched = results.iter().filter(|r| !r.candidates.is_empty()).count();
            Ok(JobOutput {
                status: format!("Found matches for {matched} of {} tracks", results.len()),
                data: JobData::Identified(results),
            })
        });
    }

    /// A callback for long jobs: publishes progress and reports whether to keep going.
    fn progress_callback(
        &self,
        ctx: &egui::Context,
    ) -> impl FnMut(usize, usize) -> bool + Send + 'static {
        let (text, cancel, ctx) = (self.progress.clone(), self.cancel.clone(), ctx.clone());
        move |done, total| {
            if let Ok(mut text) = text.lock() {
                *text = format!("Identifying {} of {total}…", done + 1);
            }
            ctx.request_repaint();
            !cancel.load(Ordering::Relaxed)
        }
    }

    fn apply_review(&mut self, ctx: &egui::Context, accepted: Vec<Accepted>, embed_cover: bool) {
        self.review = None;
        self.view = View::Library;
        self.run_job(ctx, "Saving tags", move |library| {
            let n = accepted.len();
            let mut covers: HashMap<String, Option<Vec<u8>>> = HashMap::new();
            let mut lookup = Lookup::new(Endpoints::default(), None)?;
            let edits = accepted
                .into_iter()
                .map(|a| {
                    let cover = if embed_cover {
                        covers
                            .entry(a.release)
                            .or_insert_with_key(|release| lookup.cover_art(release))
                            .clone()
                    } else {
                        None
                    };
                    TrackEdit {
                        track_id: a.track_id,
                        changes: a.changes,
                        cover,
                    }
                })
                .collect();
            let description = format!("Identify {n} track{}", if n == 1 { "" } else { "s" });
            let (_, report) = library.apply_edits(&description, edits)?;
            Ok(batch_summary("Saved", &report).into())
        });
    }

    fn start_preview(&mut self, ctx: &egui::Context) {
        let Some(track) = self.selected_track() else {
            return;
        };
        let path = track.path.clone();
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        thread::spawn(move || {
            // The receiver is gone only if the app closed.
            let _ = tx.send(decode_preview(&path, PREVIEW_SECONDS));
            ctx.request_repaint();
        });
        self.decoding = Some(rx);
    }

    fn selected_track(&self) -> Option<&Track> {
        let id = self.selection.ids.first()?;
        self.tracks.iter().find(|t| t.id == *id)
    }

    fn poll_preview(&mut self) {
        let Some(rx) = &self.decoding else { return };
        let Ok(decoded) = rx.try_recv() else { return };
        self.decoding = None;
        if let Err(err) = decoded.and_then(|pcm| self.player.play(pcm)) {
            self.status = format!("Cannot preview this track: {err}");
        }
    }

    fn identify_view(&mut self, ui: &mut egui::Ui, busy: bool) {
        let mut accepted = None;
        let mut embed_cover = true;
        egui::CentralPanel::default().show(ui, |ui| match &mut self.review {
            Some(review) => {
                accepted = review.show(ui, busy);
                embed_cover = review.embed_cover;
            }
            None => {
                ui.centered_and_justified(|ui| {
                    ui.label("Select tracks in the Library and press Identify")
                });
            }
        });
        if let Some(accepted) = accepted {
            self.apply_review(ui.ctx(), accepted, embed_cover);
        }
    }

    fn add_to_playlist(&mut self, id: i64) {
        let ids: Vec<i64> = self.selection.ids.iter().copied().collect();
        self.status = match self.library.add_to_playlist(id, &ids) {
            Ok(()) => format!("Added {} to the playlist", ids.len()),
            Err(err) => format!("Cannot add to the playlist: {err}"),
        };
        if let Err(err) = self.playlists.refresh(&self.library) {
            self.status = format!("Cannot load playlists: {err}");
        }
    }

    fn playlists_view(&mut self, ui: &mut egui::Ui) {
        if let Some(message) = self.playlists.show(ui, &mut self.library) {
            self.status = message;
        }
    }

    fn settings_view(&mut self, ui: &mut egui::Ui) {
        let library = &self.library;
        let settings = &mut self.settings;
        let mut error = None;
        egui::CentralPanel::default().show(ui, |ui| error = settings.show(ui, library));
        if let Some(error) = error {
            self.status = error;
        }
    }

    fn preview_organize(&mut self, ctx: &egui::Context, template: Template) {
        if let Err(err) = self
            .library
            .set_setting(TEMPLATE_SETTING, &self.organize.source)
        {
            self.status = format!("Cannot save the template: {err}");
        }
        let ids = self.scope_ids();
        self.run_job(ctx, "Planning", move |library| {
            let plan = library.plan_organize(&template, ids.as_ref())?;
            Ok(JobOutput {
                status: format!("{} files would move", plan.len()),
                data: JobData::Plan(plan),
            })
        });
    }

    fn apply_organize(&mut self, ctx: &egui::Context, plan: Vec<PlannedMove>) {
        self.run_job(ctx, "Moving files", move |library| {
            let (_, report) = library.organize(plan)?;
            Ok(batch_summary("Moved", &report).into())
        });
    }

    /// The selected tracks, or `None` for the whole library.
    fn scope_ids(&self) -> Option<BTreeSet<i64>> {
        (!self.selection.ids.is_empty()).then(|| self.selection.ids.clone())
    }

    fn organize_view(&mut self, ui: &mut egui::Ui, busy: bool) {
        let ids = self.scope_ids();
        let in_scope: Vec<&Track> = self
            .tracks
            .iter()
            .filter(|t| ids.as_ref().is_none_or(|ids| ids.contains(&t.id)))
            .collect();
        let scope = match in_scope.len() {
            1 => "1 selected track".to_owned(),
            n if ids.is_some() => format!("{n} selected tracks"),
            _ => "all tracks".to_owned(),
        };
        let mut action = organize::Action::None;
        egui::CentralPanel::default().show(ui, |ui| {
            action = self.organize.show(ui, &in_scope, &scope, busy);
        });
        match action {
            organize::Action::None => {}
            organize::Action::Preview(template) => self.preview_organize(ui.ctx(), template),
            organize::Action::Apply(plan) => self.apply_organize(ui.ctx(), plan),
        }
    }

    fn library_view(&mut self, ui: &mut egui::Ui, busy: bool) {
        if self.editor.as_ref().map(Editor::ids) != Some(&self.selection.ids) {
            self.editor = (!self.selection.ids.is_empty())
                .then(|| Editor::new(&self.tracks, &self.selection.ids));
        }
        let mut action = editor::Action::None;
        let playing = self.player.is_playing();
        if let Some(editor) = &mut self.editor {
            let playlists = &self.playlists.all;
            egui::Panel::right("editor")
                .show(ui, |ui| action = editor.show(ui, busy, playing, playlists));
        }
        match action {
            editor::Action::None => {}
            editor::Action::Apply => self.apply_edits(ui.ctx()),
            editor::Action::Identify => self.identify(ui.ctx()),
            editor::Action::Preview => self.start_preview(ui.ctx()),
            editor::Action::Stop => self.player.stop(),
            editor::Action::AddToPlaylist(id) => self.add_to_playlist(id),
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
        self.poll_preview();
        if self.player.is_playing() {
            // Nothing else repaints when the clip ends and the button must flip back.
            ui.ctx().request_repaint_after(Duration::from_millis(500));
        }
        let busy = self.job.is_some();

        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.view, View::Library, "Library");
                ui.selectable_value(&mut self.view, View::Organize, "Organize");
                ui.selectable_value(&mut self.view, View::Identify, "Review");
                ui.selectable_value(&mut self.view, View::Playlists, "Playlists");
                ui.selectable_value(&mut self.view, View::History, "History");
                ui.selectable_value(&mut self.view, View::Settings, "Settings");
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
                let progress = self.progress.lock().map(|p| p.clone()).unwrap_or_default();
                if busy && !progress.is_empty() {
                    ui.label(progress);
                    if ui.button("Cancel").clicked() {
                        self.cancel.store(true, Ordering::Relaxed);
                    }
                } else {
                    ui.label(&self.status);
                }
            });
        });

        egui::Panel::bottom("status").show(ui, |ui| {
            let n = self.tracks.len();
            ui.label(format!("{n} track{}", if n == 1 { "" } else { "s" }));
        });

        match self.view {
            View::Library => self.library_view(ui, busy),
            View::Organize => self.organize_view(ui, busy),
            View::Identify => self.identify_view(ui, busy),
            View::Playlists => self.playlists_view(ui),
            View::History => self.history_view(ui, busy),
            View::Settings => self.settings_view(ui),
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

        harness.get_by_label("Organize").click();
        harness.run();
        harness.get_by_label("Organize 1 selected track");
        harness.get_by_label("Preview moves");

        harness.get_by_label("Playlists").click();
        harness.run();
        harness.get_by_label("No playlists yet");
        harness.get_by_label("New playlist");

        harness.get_by_label("Settings").click();
        harness.run();
        harness.get_by_label("Allow online lookups");

        harness.get_by_label("Library").click();
        harness.run();
        harness.get_by_label("Identify").click();
        harness.run();
        harness.get_by_label("AcoustID API key");
    }
}
