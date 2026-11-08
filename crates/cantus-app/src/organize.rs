use cantus_core::{PlannedMove, Template, Track};
use eframe::egui::{self, Ui};

const SAMPLES: usize = 3;
const ROW_HEIGHT: f32 = 18.0;

pub struct Organize {
    pub source: String,
    pub plan: Option<Vec<PlannedMove>>,
}

pub enum Action {
    None,
    Preview(Template),
    Apply(Vec<PlannedMove>),
}

impl Organize {
    pub fn new(source: String) -> Self {
        Self { source, plan: None }
    }

    /// `tracks` are the tracks in scope, used for the live examples.
    pub fn show(&mut self, ui: &mut Ui, tracks: &[&Track], scope: &str, busy: bool) -> Action {
        ui.heading(format!("Organize {scope}"));
        ui.add_space(4.0);
        let label = ui.label("Path template");
        let edit = ui
            .add(
                egui::TextEdit::singleline(&mut self.source)
                    .desired_width(f32::INFINITY)
                    .font(egui::TextStyle::Monospace),
            )
            .labelled_by(label.id);
        if edit.changed() {
            self.plan = None;
        }
        ui.small(
            "{title} {artist} {albumartist} {album} {track} {disc} {year} {genre} {ext}   \
             {track:02} pads   {a|b} falls back   [ ] drops if a field inside is empty",
        );
        ui.add_space(8.0);

        let template = Template::parse(&self.source);
        match &template {
            Ok(template) => {
                for track in tracks.iter().take(SAMPLES) {
                    ui.monospace(template.render(track).display().to_string());
                }
            }
            Err(err) => {
                ui.colored_label(ui.visuals().error_fg_color, err);
            }
        }
        ui.add_space(8.0);

        let mut action = Action::None;
        ui.horizontal(|ui| {
            let can_preview = !busy && template.is_ok();
            if ui
                .add_enabled(can_preview, egui::Button::new("Preview moves"))
                .clicked()
                && let Ok(template) = template
            {
                action = Action::Preview(template);
            }
            if let Some(plan) = &self.plan {
                let n = plan.len();
                let text = format!("Apply {n} move{}", if n == 1 { "" } else { "s" });
                if ui
                    .add_enabled(!busy && n > 0, egui::Button::new(text))
                    .clicked()
                {
                    action = Action::Apply(plan.clone());
                }
            }
        });
        ui.add_space(8.0);

        if let Some(plan) = &self.plan {
            show_plan(ui, plan);
        }
        action
    }
}

fn show_plan(ui: &mut Ui, plan: &[PlannedMove]) {
    let collisions = plan.iter().filter(|m| m.collision).count();
    if plan.is_empty() {
        ui.label("Everything is already in place.");
    } else if collisions > 0 {
        ui.colored_label(
            ui.visuals().warn_fg_color,
            format!("{collisions} files would collide and get a numbered suffix."),
        );
    }
    egui::ScrollArea::both().auto_shrink(false).show_rows(
        ui,
        ROW_HEIGHT,
        plan.len(),
        |ui, rows| {
            for m in &plan[rows] {
                let relative = |p: &'_ std::path::Path| {
                    p.strip_prefix(&m.root).unwrap_or(p).display().to_string()
                };
                let text = format!("{}  to  {}", relative(&m.from), relative(&m.to));
                if m.collision {
                    ui.colored_label(ui.visuals().warn_fg_color, text);
                } else {
                    ui.label(text);
                }
            }
        },
    );
}
