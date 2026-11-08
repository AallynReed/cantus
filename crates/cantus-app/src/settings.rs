use cantus_core::Library;
use eframe::egui::{self, Ui};

const ONLINE_LOOKUPS: &str = "online_lookups";
const ACOUSTID_KEY: &str = "acoustid_key";

/// A key baked in at build time, for official builds.
const BUILT_IN_ACOUSTID_KEY: Option<&str> = option_env!("CANTUS_ACOUSTID_KEY");

pub struct Settings {
    pub online_lookups: bool,
    acoustid_key: String,
}

impl Settings {
    pub fn load(library: &Library) -> Self {
        let read = |key| library.setting(key).ok().flatten();
        Self {
            online_lookups: read(ONLINE_LOOKUPS).as_deref() == Some("on"),
            acoustid_key: read(ACOUSTID_KEY).unwrap_or_default(),
        }
    }

    /// Shows the page. Returns an error message if saving failed.
    pub fn show(&mut self, ui: &mut Ui, library: &Library) -> Option<String> {
        let mut result = Ok(());
        ui.heading("Online lookups");
        ui.add_space(4.0);
        if ui
            .checkbox(&mut self.online_lookups, "Allow online lookups")
            .changed()
        {
            let value = if self.online_lookups { "on" } else { "off" };
            result = library.set_setting(ONLINE_LOOKUPS, value);
        }
        ui.label(
            "Identify sends audio fingerprints, durations and tag text to AcoustID and \
             MusicBrainz, and fetches cover art from the Cover Art Archive. It never sends file \
             paths or anything about you. Cantus has no servers of its own and no telemetry.",
        );
        ui.add_space(12.0);

        let label = ui.label("AcoustID API key");
        let edit = ui
            .add(egui::TextEdit::singleline(&mut self.acoustid_key).desired_width(280.0))
            .labelled_by(label.id);
        if edit.changed() {
            result = library.set_setting(ACOUSTID_KEY, self.acoustid_key.trim());
        }
        ui.small(
            "Needed to match by audio; without one, Cantus searches by the existing tags. \
             Get a free key at acoustid.org.",
        );
        result
            .err()
            .map(|err| format!("Cannot save settings: {err}"))
    }
}

/// The key to use for lookups: the user's, or the one built in.
pub fn acoustid_key(library: &Library) -> Option<String> {
    library
        .setting(ACOUSTID_KEY)
        .ok()
        .flatten()
        .filter(|k| !k.is_empty())
        .or(BUILT_IN_ACOUSTID_KEY.map(str::to_owned))
}
