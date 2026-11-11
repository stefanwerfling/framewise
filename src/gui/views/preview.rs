//! Confirmation dialog before jobs are added: shows target paths, naming
//! conflicts, replacements and every warning of the encode plans.

use eframe::egui::{self, RichText};

use crate::core::events::{Command, JobRequest};
use crate::core::paths::ConflictResolution;
use crate::gui::{FramewiseApp, theme};
use crate::{tr, trf};

pub fn show(app: &mut FramewiseApp, ctx: &egui::Context) {
    let Some(preview) = app.preview.as_mut() else {
        return;
    };
    let mut close = false;
    let mut submit: Option<Vec<JobRequest>> = None;

    let blocked = preview
        .items
        .iter()
        .filter(|i| !i.blockers.is_empty())
        .count();
    let queued = preview.items.iter().filter(|i| i.already_queued).count();
    let skipped = preview
        .items
        .iter()
        .filter(|i| matches!(i.resolution, ConflictResolution::Skip { .. }))
        .count();
    let renamed = preview
        .items
        .iter()
        .filter(|i| matches!(i.resolution, ConflictResolution::Renamed { .. }))
        .count();
    let replaced = preview
        .items
        .iter()
        .filter(|i| matches!(i.resolution, ConflictResolution::Replace(_)))
        .count();
    let with_warnings = preview
        .items
        .iter()
        .filter(|i| !i.warnings.is_empty())
        .count();

    let profile_name = app
        .snap
        .profiles
        .iter()
        .find(|p| p.id == preview.profile_id)
        .map_or_else(|| preview.profile_id.clone(), |p| p.name.clone());
    egui::Modal::new(egui::Id::unique("job_preview")).show(ctx, |ui| {
        ui.set_width(ctx.content_rect().width().min(980.0) - 60.0);
        ui.heading(tr!("Add to queue"));
        ui.label(trf!(
            "{} files · profile {}",
            preview.items.len(),
            profile_name
        ));
        ui.horizontal_wrapped(|ui| {
            if skipped > 0 {
                ui.label(RichText::new(trf!("{} skipped (target exists)", skipped)).color(theme::WARN));
            }
            if renamed > 0 {
                ui.label(RichText::new(trf!("{} renamed", renamed)).color(theme::ACCENT));
            }
            if replaced > 0 {
                ui.label(RichText::new(trf!("{} replace an existing result", replaced)).color(theme::ERROR));
            }
            if blocked > 0 {
                ui.label(RichText::new(trf!("{} cannot be processed", blocked)).color(theme::ERROR));
            }
            if queued > 0 {
                ui.label(RichText::new(trf!("{} already in queue", queued)).color(theme::MUTED));
            }
            if with_warnings > 0 {
                ui.label(RichText::new(trf!("{} with notes – please review", with_warnings)).color(theme::WARN));
            }
        });
        ui.label(
            RichText::new(tr!("Original files are never modified. Results are written to a work directory first and moved into place only after successful validation."))
                .small()
                .color(theme::MUTED),
        );
        ui.separator();

        egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
            for item in &preview.items {
                let (icon, color, target) = match &item.resolution {
                    ConflictResolution::Use(p) => ("✔", theme::OK, p.display().to_string()),
                    ConflictResolution::Renamed { renamed, .. } => ("✎", theme::ACCENT, renamed.display().to_string()),
                    ConflictResolution::Replace(p) => ("⚠", theme::ERROR, trf!("{} (replace)", p.display())),
                    ConflictResolution::Skip { existing } => ("⏭", theme::WARN, trf!("{} exists – skipped", existing.display())),
                };
                let (icon, color) = if !item.blockers.is_empty() {
                    ("⛔", theme::ERROR)
                } else if item.already_queued {
                    ("⏳", theme::MUTED)
                } else {
                    (icon, color)
                };
                ui.horizontal(|ui| {
                    ui.label(RichText::new(icon).color(color));
                    ui.label(RichText::new(item.relative.display().to_string()).strong());
                    ui.label(RichText::new(item.recommendation.label()).small().color(theme::MUTED));
                });
                ui.label(RichText::new(format!("   → {target}")).small());
                for b in &item.blockers {
                    ui.label(RichText::new(format!("   ⛔ {}", b.describe())).small().color(theme::ERROR));
                }
                for w in &item.warnings {
                    ui.label(RichText::new(format!("   ⚠ {}", w.describe())).small().color(theme::WARN));
                }
            }
        });
        ui.separator();

        if replaced > 0 {
            ui.checkbox(
                &mut preview.confirm_replace,
                RichText::new(trf!(
                    "I confirm that {} existing result file(s) may be replaced (after successful validation)",
                    replaced
                ))
                .color(theme::ERROR),
            );
        }

        let requests: Vec<JobRequest> = preview
            .items
            .iter()
            .filter(|i| i.blockers.is_empty() && !i.already_queued)
            .filter_map(|i| match &i.resolution {
                ConflictResolution::Use(p) | ConflictResolution::Renamed { renamed: p, .. } => Some(JobRequest {
                    source: i.source.clone(),
                    target: p.clone(),
                    replace_existing: false,
                }),
                ConflictResolution::Replace(p) if preview.confirm_replace => Some(JobRequest {
                    source: i.source.clone(),
                    target: p.clone(),
                    replace_existing: true,
                }),
                _ => None,
            })
            .collect();

        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !requests.is_empty(),
                    egui::Button::new(RichText::new(trf!("Add {} job(s)", requests.len())).strong()),
                )
                .clicked()
            {
                submit = Some(requests);
            }
            if ui.button(tr!("Cancel")).clicked() {
                close = true;
            }
        });
    });

    if let Some(items) = submit {
        let profile_id = preview.profile_id.clone();
        app.send(Command::AddJobs { profile_id, items });
        close = true;
    }
    if close {
        app.preview = None;
    }
}
