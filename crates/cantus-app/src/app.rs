use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cantus_core::{
    BatchReport, ChangeSetSummary, DEFAULT_TEMPLATE, Endpoints, Handoff, Identified, Library,
    Lookup, Pcm, PlannedMove, ScanReport, SyncPlan, Template, TorrentSource, Torrents, Track,
    TrackEdit, decode_preview, download_audio, preview_sync, run_sync,
};
use eframe::egui;

use crate::editor::{self, Editor};
use crate::import::{self, Import};
use crate::organize::{self, Organize};
use crate::platform;
use crate::player::Player;
use crate::playlists::Playlists;
use crate::review::{Accepted, Review};
use crate::settings::{self, Settings};
use crate::sync::{self, Sync};
use crate::table::{self, Selection, Sort, SortKey};

const TEMPLATE_SETTING: &str = "template";
const PREVIEW_SECONDS: u32 = 60;
const TORRENT_POLL: Duration = Duration::from_secs(1);
const ACCESS_CHECK: Duration = Duration::from_secs(2);
const YT_DLP: &str = "yt-dlp";
const RCLONE: &str = "rclone";
const SNAPSHOT_NAME: &str = ".cantus-library.db";

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
    ImportPlan(i64, Vec<PlannedMove>),
    SyncPlan(SyncPlan),
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
    Import,
    Playlists,
    Sync,
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
    import: Import,
    sync: Sync,
    roots: Vec<PathBuf>,
    torrents: Option<Arc<Torrents>>,
    starting_torrents: Option<Receiver<cantus_core::Result<Torrents>>>,
    adding_torrent: Option<Receiver<Result<(), String>>>,
    last_torrent_poll: Option<Instant>,
    playlists: Playlists,
    torrents_started: bool,
    file_access: bool,
    last_access_check: Option<Instant>,
    delete_data: Arc<AtomicBool>,
    confirm_delete: bool,
    scroll_to: Option<usize>,
    settings: Settings,
    player: Player,
    decoding: Option<Receiver<Result<Pcm, String>>>,
    status: String,
}

impl App {
    pub fn new(db_path: PathBuf, mut library: Library, delete_data: Arc<AtomicBool>) -> Self {
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
            import: Import::default(),
            sync: Sync::default(),
            roots: Vec::new(),
            torrents: None,
            starting_torrents: None,
            adding_torrent: None,
            last_torrent_poll: None,
            playlists: Playlists::default(),
            torrents_started: false,
            file_access: true,
            last_access_check: None,
            delete_data,
            confirm_delete: false,
            scroll_to: None,
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
                self.selection.forget_rows();
                self.editor = None;
                self.organize.plan = None;
                if let Err(err) = self.playlists.refresh(&self.library) {
                    self.status = format!("Cannot load playlists: {err}");
                }
                self.roots = self.library.roots().unwrap_or_default();
                self.import.batches = self.library.inbox().unwrap_or_default();
                self.import.plan = None;
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
                    JobData::ImportPlan(batch, plan) => self.import.plan = Some((batch, plan)),
                    JobData::SyncPlan(plan) => self.sync.plan = Some(plan),
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

    fn start_torrents(&mut self, ctx: &egui::Context) {
        if self.torrents.is_some() || self.starting_torrents.is_some() {
            return;
        }
        let state_dir = self.db_path.with_file_name("torrents");
        let config = self.settings.torrent_config(state_dir);
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        thread::spawn(move || {
            // The receiver is gone only if the app closed.
            let _ = tx.send(Torrents::start(config));
            ctx.request_repaint();
        });
        self.starting_torrents = Some(rx);
    }

    /// Picks up engine start-up, finished adds, and download progress once a second.
    fn poll_torrents(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.starting_torrents
            && let Ok(started) = rx.try_recv()
        {
            self.starting_torrents = None;
            match started {
                Ok(torrents) => self.torrents = Some(Arc::new(torrents)),
                Err(err) => self.status = format!("Cannot start the torrent engine: {err}"),
            }
        }
        if let Some(rx) = &self.adding_torrent
            && let Ok(added) = rx.try_recv()
        {
            self.adding_torrent = None;
            self.status = match added {
                Ok(()) => "Torrent added. Pick its files and press Start.".to_owned(),
                Err(err) => err,
            };
            self.last_torrent_poll = None;
        }
        let Some(torrents) = self.torrents.clone() else {
            return;
        };
        ctx.request_repaint_after(TORRENT_POLL);
        if self
            .last_torrent_poll
            .is_some_and(|at| at.elapsed() < TORRENT_POLL)
        {
            return;
        }
        self.last_torrent_poll = Some(Instant::now());
        self.import.torrent_files = platform::torrent_files();
        self.import.statuses = torrents.list();
        self.import.files = self
            .import
            .statuses
            .iter()
            .map(|s| (s.id, torrents.files(s.id)))
            .collect();
        match self.library.collect_finished(&torrents) {
            Ok(names) if !names.is_empty() => {
                self.status = format!("Ready to import: {}", names.join(", "));
                self.import.batches = self.library.inbox().unwrap_or_default();
            }
            Ok(_) => {}
            Err(err) => self.status = format!("Cannot update the Inbox: {err}"),
        }
    }

    fn add_link(&mut self, ctx: &egui::Context, link: String) {
        if link.starts_with("magnet:") {
            self.add_torrent(ctx, TorrentSource::Magnet(link));
        } else {
            self.import_url(ctx, link);
        }
    }

    fn add_torrent(&mut self, ctx: &egui::Context, source: TorrentSource) {
        let Some(torrents) = self.torrents.clone() else {
            self.status = "Enable the torrent engine in Settings first".into();
            self.view = View::Settings;
            return;
        };
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let added = torrents.add(source).map(drop).map_err(|e| e.to_string());
            // The receiver is gone only if the app closed.
            let _ = tx.send(added);
            ctx.request_repaint();
        });
        self.adding_torrent = Some(rx);
        self.status = "Adding torrent…".into();
    }

    fn import_url(&mut self, ctx: &egui::Context, url: String) {
        let folder = format!("web-{}", unix_now());
        let dest = self.settings.download_dir.join(folder);
        self.run_job(ctx, "Downloading", move |library| {
            std::fs::create_dir_all(&dest)?;
            let files = download_audio(Path::new(YT_DLP), &url, &dest)
                .map_err(cantus_core::Error::InvalidInput)?;
            let name = files[0]
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            library.add_to_inbox("web", &name, &files)?;
            Ok(format!("Ready to import: {name}").into())
        });
    }

    fn plan_import(&mut self, ctx: &egui::Context, batch: i64) {
        let Some(root) = self.roots.first().cloned() else {
            self.status = "Add a library folder first".into();
            return;
        };
        let template = match Template::parse(&self.organize.source) {
            Ok(template) => template,
            Err(err) => {
                self.status = format!("Fix the path template in Organize first: {err}");
                return;
            }
        };
        self.run_job(ctx, "Planning", move |library| {
            let plan = library.plan_import(batch, &root, &template)?;
            Ok(JobOutput {
                status: format!("{} files ready to import", plan.len()),
                data: JobData::ImportPlan(batch, plan),
            })
        });
    }

    fn apply_import(&mut self, ctx: &egui::Context, batch: i64, plan: Vec<PlannedMove>) {
        let handoff = if self.import.hardlink {
            Handoff::Hardlink
        } else {
            Handoff::Copy
        };
        self.run_job(ctx, "Importing", move |library| {
            let (_, report) = library.import_inbox(batch, plan, handoff)?;
            Ok(batch_summary("Imported", &report).into())
        });
    }

    fn import_view(&mut self, ui: &mut egui::Ui, busy: bool) {
        let mut action = import::Action::None;
        let on = self.torrents.is_some();
        egui::CentralPanel::default().show(ui, |ui| action = self.import.show(ui, on, busy));
        let ctx = ui.ctx().clone();
        let torrents = self.torrents.clone();
        let outcome = match action {
            import::Action::None => Ok(()),
            import::Action::Add(link) => {
                self.add_link(&ctx, link);
                Ok(())
            }
            import::Action::AddFile(file) => match std::fs::read(&file) {
                Ok(bytes) => {
                    self.add_torrent(&ctx, TorrentSource::File(bytes));
                    Ok(())
                }
                Err(err) => Err(err.to_string()),
            },
            import::Action::Plan(batch) => {
                self.plan_import(&ctx, batch);
                Ok(())
            }
            import::Action::Apply(batch, plan) => {
                self.apply_import(&ctx, batch, plan);
                Ok(())
            }
            import::Action::Dismiss(batch) => {
                let result = self.library.dismiss_inbox(batch).map_err(|e| e.to_string());
                self.import.batches = self.library.inbox().unwrap_or_default();
                result
            }
            import::Action::Resume(id) => with_engine(torrents, |t| t.resume(id)),
            import::Action::Pause(id) => with_engine(torrents, |t| t.pause(id)),
            import::Action::Remove(id) => with_engine(torrents, |t| t.remove(id, false)),
            import::Action::Select(id, files) => {
                with_engine(torrents, |t| t.select_files(id, &files))
            }
        };
        if let Err(err) = outcome {
            self.status = err;
        }
    }

    fn sync_view(&mut self, ui: &mut egui::Ui, busy: bool) {
        let mut action = sync::Action::None;
        let roots = &self.roots;
        let sync = &mut self.sync;
        egui::CentralPanel::default().show(ui, |ui| action = sync.show(ui, roots, busy));
        match action {
            sync::Action::None => {}
            sync::Action::Preview(root) => {
                let (remote, mirror) = (self.sync.remote.trim().to_owned(), self.sync.mirror);
                self.run_job(ui.ctx(), "Checking", move |_| {
                    let plan = preview_sync(Path::new(RCLONE), &root, &remote, mirror)
                        .map_err(cantus_core::Error::InvalidInput)?;
                    Ok(JobOutput {
                        status: "Dry run finished".to_owned(),
                        data: JobData::SyncPlan(plan),
                    })
                });
            }
            sync::Action::Run(root, plan) => {
                self.sync.plan = None;
                self.run_job(ui.ctx(), "Syncing", move |library| {
                    library.snapshot(&root.join(SNAPSHOT_NAME))?;
                    run_sync(Path::new(RCLONE), &plan).map_err(cantus_core::Error::InvalidInput)?;
                    Ok("Sync finished".to_owned().into())
                });
            }
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
        let mut change = settings::Change::None;
        egui::CentralPanel::default().show(ui, |ui| change = settings.show(ui, library));
        match change {
            settings::Change::None => {}
            settings::Change::Failed(error) => self.status = error,
            settings::Change::DeleteAllData => self.confirm_delete = true,
            settings::Change::Torrents(true) => self.start_torrents(ui.ctx()),
            settings::Change::Torrents(false) => {
                self.torrents = None;
                self.import.statuses.clear();
            }
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

    /// Up, Down, Ctrl+A and Esc, unless a text field has the keyboard.
    fn handle_table_keys(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let (delta, all, escape, modifiers) = ctx.input(|i| {
            let delta = isize::from(i.key_pressed(egui::Key::ArrowDown))
                - isize::from(i.key_pressed(egui::Key::ArrowUp));
            (
                delta,
                i.modifiers.command && i.key_pressed(egui::Key::A),
                i.key_pressed(egui::Key::Escape),
                i.modifiers,
            )
        });
        if delta != 0 {
            self.scroll_to = self.selection.step(&self.tracks, delta, modifiers);
        }
        if all {
            self.selection.select_all(&self.tracks);
        }
        if escape {
            self.selection.clear();
        }
    }

    fn confirm_delete_dialog(&mut self, ctx: &egui::Context) {
        if !self.confirm_delete {
            return;
        }
        egui::Modal::new(egui::Id::unique("confirm_delete")).show(ctx, |ui| {
            ui.heading("Delete all Cantus data?");
            ui.label("This removes the library database, history, playlists, settings and backups, then closes Cantus. Your music and downloads are not touched. It cannot be undone.");
            ui.horizontal(|ui| {
                if ui.button("Keep my data").clicked() {
                    self.confirm_delete = false;
                }
                if ui.button("Delete and close").clicked() {
                    self.delete_data.store(true, Ordering::Relaxed);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        });
    }

    fn library_view(&mut self, ui: &mut egui::Ui, busy: bool) {
        self.handle_table_keys(ui.ctx());
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
            let scroll_to = self.scroll_to.take();
            if let Some(key) =
                table::show(ui, &self.tracks, self.sort, &mut self.selection, scroll_to)
            {
                self.sort.toggle(key);
                self.sort.apply(&mut self.tracks);
                self.selection.forget_rows();
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

fn with_engine(
    torrents: Option<Arc<Torrents>>,
    action: impl FnOnce(&Torrents) -> cantus_core::Result<()>,
) -> Result<(), String> {
    torrents.map_or(Ok(()), |t| action(&t).map_err(|e| e.to_string()))
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
        platform::sync_keyboard(ui.ctx().egui_wants_keyboard_input());
        if self
            .last_access_check
            .is_none_or(|at| at.elapsed() >= ACCESS_CHECK)
        {
            self.last_access_check = Some(Instant::now());
            self.file_access = platform::has_file_access();
        }
        if !self.file_access {
            // Nothing else repaints when the user returns from Android's settings.
            ui.ctx().request_repaint_after(ACCESS_CHECK);
        }
        self.poll_job();
        self.poll_preview();
        self.poll_torrents(ui.ctx());
        if self.settings.torrents && !self.torrents_started {
            self.torrents_started = true;
            self.start_torrents(ui.ctx());
        }
        if self.player.is_playing() {
            // Nothing else repaints when the clip ends and the button must flip back.
            ui.ctx().request_repaint_after(Duration::from_millis(500));
        }
        let busy = self.job.is_some();

        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.selectable_value(&mut self.view, View::Library, "Library");
                ui.selectable_value(&mut self.view, View::Organize, "Organize");
                ui.selectable_value(&mut self.view, View::Identify, "Review");
                ui.selectable_value(&mut self.view, View::Import, "Import");
                ui.selectable_value(&mut self.view, View::Playlists, "Playlists");
                if platform::DESKTOP {
                    ui.selectable_value(&mut self.view, View::Sync, "Sync");
                }
                ui.selectable_value(&mut self.view, View::History, "History");
                ui.selectable_value(&mut self.view, View::Settings, "Settings");
                ui.separator();
                if !self.file_access && ui.button("Allow file access").clicked() {
                    platform::request_file_access();
                }
                let add = ui.add_enabled(!busy, egui::Button::new("Add folder…"));
                if add.clicked()
                    && let Some(dir) = platform::pick_folder()
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
            View::Import => self.import_view(ui, busy),
            View::Playlists => self.playlists_view(ui),
            View::Sync => self.sync_view(ui, busy),
            View::History => self.history_view(ui, busy),
            View::Settings => self.settings_view(ui),
        }
        self.confirm_delete_dialog(ui.ctx());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::{Harness, kittest::Queryable};

    #[test]
    fn starts_with_empty_library() {
        let library = Library::open_in_memory().unwrap();
        let harness = Harness::new_eframe(|_| App::new(PathBuf::new(), library, Arc::default()));
        harness.get_by_label("Add folder…");
        harness.get_by_label("Library empty");
        harness.get_by_label("0 tracks");
    }

    #[test]
    fn background_scan_results_show_after_reload() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = cantus_core::test_fixtures().join("tone.flac");
        std::fs::copy(fixture, dir.path().join("tone.flac")).unwrap();
        let db_path = dir.path().join("library.db");

        let mut library = Library::open(&db_path).unwrap();
        let report = scan(&mut library, Some(dir.path().to_path_buf())).unwrap();
        assert_eq!(
            summary(&report),
            "1 added, 0 updated, 0 moved, 0 missing, 0 unchanged"
        );

        let mut harness =
            Harness::new_eframe(|_| App::new(db_path.clone(), library, Arc::default()));
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

        harness.get_by_label("Import").click();
        harness.run();
        harness.get_by_label("No torrents");
        harness.get_by_label("Nothing waiting to be imported");

        harness.get_by_label("Sync").click();
        harness.run();
        harness.get_by_label("Dry run");

        harness.get_by_label("Playlists").click();
        harness.run();
        harness.get_by_label("No playlists yet");
        harness.get_by_label("New playlist");

        harness.get_by_label("Settings").click();
        harness.run();
        harness.get_by_label("Allow online lookups");
        harness.get_by_label("Delete all Cantus data…").click();
        harness.run();
        harness.get_by_label("Keep my data").click();
        harness.run();

        harness.get_by_label("Library").click();
        harness.run();
        harness.get_by_label("Identify").click();
        harness.run();
        harness.get_by_label("AcoustID API key");
    }
}
