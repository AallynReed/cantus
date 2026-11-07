#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;

fn main() -> eframe::Result {
    eframe::run_native(
        concat!("Cantus ", env!("CARGO_PKG_VERSION")),
        eframe::NativeOptions::default(),
        Box::new(|_| Ok(Box::new(App))),
    )
}

struct App;

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, library_view);
    }
}

fn library_view(ui: &mut egui::Ui) {
    ui.centered_and_justified(|ui| ui.label("Library empty"));
}

#[cfg(test)]
mod tests {
    use egui_kittest::{Harness, kittest::Queryable};

    #[test]
    fn empty_library_shows_placeholder() {
        let harness = Harness::new_ui(super::library_view);
        harness.get_by_label("Library empty");
    }
}
