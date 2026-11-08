use std::path::PathBuf;

use cantus_core::SyncPlan;
use eframe::egui::{self, Ui};

/// The Sync tab. A sync only becomes available after its dry run has been shown.
#[derive(Default)]
pub struct Sync {
    pub remote: String,
    pub mirror: bool,
    root: usize,
    pub plan: Option<SyncPlan>,
}

pub enum Action {
    None,
    Preview(PathBuf),
    Run(PathBuf, SyncPlan),
}

impl Sync {
    pub fn show(&mut self, ui: &mut Ui, roots: &[PathBuf], busy: bool) -> Action {
        ui.heading("Sync to cloud storage");
        ui.label("Copies your library to an rclone remote. rclone keeps your credentials; Cantus never sees them.");
        ui.add_space(8.0);
        if roots.is_empty() {
            ui.label("Add a library folder first.");
            return Action::None;
        }
        self.root = self.root.min(roots.len() - 1);

        let mut changed = false;
        let folder_label = ui.label("Folder");
        egui::ComboBox::from_id_salt("sync_root")
            .selected_text(roots[self.root].display().to_string())
            .show_ui(ui, |ui| {
                for (i, root) in roots.iter().enumerate() {
                    changed |= ui
                        .selectable_value(&mut self.root, i, root.display().to_string())
                        .changed();
                }
            })
            .response
            .labelled_by(folder_label.id);
        let label = ui.label("rclone remote, like name:music");
        changed |= ui
            .add(egui::TextEdit::singleline(&mut self.remote).desired_width(280.0))
            .labelled_by(label.id)
            .changed();
        changed |= ui
            .checkbox(
                &mut self.mirror,
                "Also delete remote files that are gone here",
            )
            .changed();
        if changed {
            self.plan = None;
        }
        ui.add_space(8.0);

        let root = roots[self.root].clone();
        let mut action = Action::None;
        let can_preview = !busy && !self.remote.trim().is_empty();
        if ui
            .add_enabled(can_preview, egui::Button::new("Dry run"))
            .clicked()
        {
            action = Action::Preview(root.clone());
        }
        if let Some(plan) = &self.plan {
            ui.label(format!(
                "Would copy {} files and delete {} on the remote.",
                plan.copies, plan.deletes
            ));
            if plan.deletes > 0 {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    "Deleted remote files cannot be restored by Cantus.",
                );
            }
            if ui
                .add_enabled(!busy, egui::Button::new("Run sync"))
                .clicked()
            {
                action = Action::Run(root, plan.clone());
            }
        }
        action
    }
}
