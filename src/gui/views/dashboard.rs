//! Dashboard: key figures of library and queue.

use eframe::egui::{self, RichText};

use crate::analyzer::recommend::Recommendation;
use crate::core::events::Command;
use crate::gui::{FramewiseApp, View, format, theme};
use crate::queue::job::JobState;
use crate::scanner::ScanProgress;
use crate::{tr, trf};

fn tile(ui: &mut egui::Ui, title: &str, value: String, hint: Option<&str>, color: egui::Color32) {
    theme::card().show(ui, |ui| {
        ui.set_min_size(egui::vec2(170.0, 64.0));
        ui.vertical(|ui| {
            ui.label(RichText::new(title).color(theme::MUTED).small());
            ui.label(RichText::new(value).size(22.0).strong().color(color));
            if let Some(h) = hint {
                ui.label(RichText::new(h).small().color(theme::MUTED));
            }
        });
    });
}

pub fn scan_progress_label(ui: &mut egui::Ui, p: &ScanProgress) {
    let text = match p {
        ScanProgress::Discovering => tr!("Searching files…").to_string(),
        ScanProgress::Discovered { total } => trf!("{} files found", total),
        ScanProgress::Probed { done, total, .. } => trf!("Analysing {} / {}", done, total),
        ScanProgress::Duplicates => tr!("Checking duplicates…").to_string(),
    };
    ui.add(egui::Spinner::new());
    ui.label(RichText::new(text).color(theme::ACCENT));
}

pub fn scan_controls(app: &mut FramewiseApp, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        if app.scan_running() {
            if ui.button(tr!("Cancel scan")).clicked() {
                app.send(Command::CancelScan);
            }
            if let Some(p) = &app.snap.scan_progress {
                if let ScanProgress::Probed { done, total, .. } = p {
                    ui.add(
                        egui::ProgressBar::new(*done as f32 / (*total).max(1) as f32)
                            .desired_width(240.0)
                            .text(format!("{done} / {total}")),
                    );
                } else {
                    scan_progress_label(ui, p);
                }
            }
        } else {
            let ready =
                app.snap.config.source_dir.is_some() && app.snap.config.target_dir.is_some();
            let resp = ui.add_enabled(
                ready,
                egui::Button::new(RichText::new(tr!("⟳ Scan source")).strong()),
            );
            if resp.clicked() {
                app.send(Command::StartScan);
            }
            if !ready {
                ui.label(
                    RichText::new(tr!("Set source and target directory in the settings first"))
                        .color(theme::WARN),
                );
                if ui.link(tr!("Open settings")).clicked() {
                    app.view = View::Settings;
                }
            }
        }
        if let Some(src) = &app.snap.config.source_dir {
            let text = format!(
                "{}  →  {}",
                src.display(),
                app.snap
                    .config
                    .target_dir
                    .as_ref()
                    .map(|t| t.display().to_string())
                    .unwrap_or_default()
            );
            ui.add(egui::Label::new(RichText::new(&text).color(theme::MUTED)).truncate())
                .on_hover_text(text);
        }
    });
}

pub fn show(app: &mut FramewiseApp, ui: &mut egui::Ui) {
    ui.heading(tr!("Dashboard"));
    scan_controls(app, ui);
    ui.add_space(8.0);

    let lib = app.snap.library.clone();
    let total_size: u64 = lib.iter().map(|e| e.media.size_bytes).sum();
    let savings: u64 = lib
        .iter()
        .filter(|e| {
            matches!(
                e.analysis.recommendation,
                Recommendation::Transcode | Recommendation::Remux
            )
        })
        .filter_map(|e| e.analysis.estimated_savings)
        .sum();
    let calibrated = app
        .snap
        .library
        .iter()
        .find_map(|e| e.analysis.calibrated_from);
    let optimized: u64 = app
        .snap
        .history
        .iter()
        .filter(|h| h.state == JobState::Done)
        .filter_map(|h| h.result_size)
        .sum();
    let optimized_src: u64 = app
        .snap
        .history
        .iter()
        .filter(|h| h.state == JobState::Done)
        .map(|h| h.source_size)
        .sum();
    let count = |s: JobState| app.snap.jobs.iter().filter(|j| j.state == s).count();
    let active = app.snap.jobs.iter().filter(|j| j.state.is_active()).count();
    let failed_hist = app
        .snap
        .history
        .iter()
        .filter(|h| h.state == JobState::Failed)
        .count();
    let done_hist = app
        .snap
        .history
        .iter()
        .filter(|h| h.state == JobState::Done)
        .count();

    ui.horizontal_wrapped(|ui| {
        tile(
            ui,
            tr!("Videos found"),
            lib.len().to_string(),
            None,
            theme::ACCENT,
        );
        tile(
            ui,
            tr!("Total source size"),
            format::bytes(total_size),
            None,
            egui::Color32::WHITE,
        );
        tile(
            ui,
            tr!("Already optimized"),
            format::bytes(optimized),
            Some(&trf!("from {}", format::bytes(optimized_src))),
            theme::OK,
        );
        tile(
            ui,
            tr!("Estimated savings"),
            format!("≈ {}", format::bytes(savings)),
            Some(&match calibrated {
                Some(n) => trf!("estimate, calibrated with {} jobs", n),
                None => tr!("estimate, not guaranteed").to_string(),
            }),
            theme::WARN,
        );
    });
    ui.horizontal_wrapped(|ui| {
        tile(
            ui,
            tr!("Waiting jobs"),
            count(JobState::Queued).to_string(),
            None,
            egui::Color32::WHITE,
        );
        tile(
            ui,
            tr!("Active jobs"),
            active.to_string(),
            None,
            theme::ACCENT,
        );
        tile(
            ui,
            tr!("Successful jobs"),
            done_hist.to_string(),
            None,
            theme::OK,
        );
        tile(
            ui,
            tr!("Failed jobs"),
            failed_hist.to_string(),
            None,
            if failed_hist > 0 {
                theme::ERROR
            } else {
                theme::MUTED
            },
        );
    });

    ui.add_space(10.0);
    ui.columns(2, |cols| {
        let ui = &mut cols[0];
        theme::card().show(ui, |ui| {
            ui.label(RichText::new(tr!("Recommendations")).strong());
            for rec in [
                Recommendation::Transcode,
                Recommendation::Remux,
                Recommendation::AlreadyOptimal,
                Recommendation::Review,
            ] {
                let n = lib
                    .iter()
                    .filter(|e| e.analysis.recommendation == rec)
                    .count();
                let size: u64 = lib
                    .iter()
                    .filter(|e| e.analysis.recommendation == rec)
                    .map(|e| e.media.size_bytes)
                    .sum();
                ui.horizontal(|ui| {
                    ui.label(format!("{n:>6}"));
                    ui.label(rec.label());
                    ui.label(RichText::new(format::bytes(size)).color(theme::MUTED));
                });
            }
            if let Some(summary) = &app.snap.scan_summary {
                ui.separator();
                if !summary.issues.is_empty() {
                    ui.collapsing(trf!("{} scan problems", summary.issues.len()), |ui| {
                        for issue in summary.issues.iter().take(200) {
                            ui.label(
                                RichText::new(format!(
                                    "{} – {}",
                                    issue.path.display(),
                                    issue.message
                                ))
                                .small()
                                .color(theme::WARN),
                            );
                        }
                    });
                }
                if !summary.duplicates.is_empty() {
                    ui.collapsing(
                        trf!("{} possible duplicates", summary.duplicates.len()),
                        |ui| {
                            for g in summary.duplicates.iter().take(200) {
                                for p in &g.paths {
                                    ui.label(RichText::new(p.display().to_string()).small());
                                }
                                ui.separator();
                            }
                        },
                    );
                }
            }
        });

        let ui = &mut cols[1];
        theme::card().show(ui, |ui| {
            ui.label(RichText::new(tr!("Largest files")).strong());
            let mut idx: Vec<usize> = (0..lib.len()).collect();
            idx.sort_by_key(|&i| std::cmp::Reverse(lib[i].media.size_bytes));
            for &i in idx.iter().take(10) {
                let e = &lib[i];
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format::bytes(e.media.size_bytes)).monospace());
                    ui.label(
                        RichText::new(e.analysis.recommendation.label())
                            .small()
                            .color(theme::MUTED),
                    );
                    // long release names are truncated; full path on hover
                    let link = ui
                        .add(
                            egui::Label::new(
                                RichText::new(&e.media.file_name).color(theme::ACCENT),
                            )
                            .truncate()
                            .sense(egui::Sense::click()),
                        )
                        .on_hover_text(e.media.relative_path.display().to_string());
                    if link.clicked() {
                        app.library_ui.focus = Some(e.media.absolute_path.clone());
                        app.view = View::Library;
                    }
                });
            }
        });
    });
}
