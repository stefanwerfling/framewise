//! Dashboard: key figures of library and queue.

use eframe::egui::{self, RichText};

use crate::analyzer::recommend::Recommendation;
use crate::core::config::SourceKind;
use crate::core::events::Command;
use crate::gui::{FramewiseApp, View, format, theme};
use crate::queue::job::JobState;
use crate::scanner::ScanProgress;
use crate::{tr, trf};

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
            let cfg = &app.snap.config;
            let jellyfin = cfg.source == SourceKind::Jellyfin;
            let ready = if jellyfin {
                cfg.jellyfin.is_configured()
            } else {
                cfg.source_dir.is_some() && cfg.target_dir.is_some()
            };
            let resp = if ready {
                ui.add(theme::primary_button(tr!("⟳ Scan source")))
            } else {
                ui.add_enabled(false, egui::Button::new(tr!("⟳ Scan source")))
            };
            if resp.clicked() {
                app.send(Command::StartScan);
            }
            if !ready {
                ui.label(
                    RichText::new(if jellyfin {
                        tr!("Enter the Jellyfin server address and API key in the settings first")
                    } else {
                        tr!("Set source and target directory in the settings first")
                    })
                    .color(theme::WARN),
                );
                if ui.link(tr!("Open settings")).clicked() {
                    app.view = View::Settings;
                }
            }
        }
        if app.snap.config.source == SourceKind::Jellyfin {
            let url = &app.snap.config.jellyfin.url;
            if !url.is_empty() {
                ui.label(RichText::new(format!("Jellyfin  ·  {url}")).color(theme::MUTED));
            }
        } else if let Some(src) = &app.snap.config.source_dir {
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

fn rec_color(rec: Recommendation) -> egui::Color32 {
    match rec {
        Recommendation::Transcode => theme::ACCENT,
        Recommendation::Remux => theme::HDR,
        Recommendation::AlreadyOptimal => theme::OK,
        Recommendation::Review => theme::WARN,
    }
}

pub fn show(app: &mut FramewiseApp, ui: &mut egui::Ui) {
    theme::page_header(
        ui,
        tr!("Dashboard"),
        Some(tr!("Overview of your library and the queue")),
        |_| {},
    );
    scan_controls(app, ui);
    ui.add_space(10.0);

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
    let done = || {
        app.snap
            .history
            .iter()
            .filter(|h| h.state == JobState::Done)
    };
    let optimized: u64 = done().filter_map(|h| h.result_size).sum();
    let optimized_src: u64 = done().map(|h| h.source_size).sum();
    let done_hist = done().count();
    let waiting = app
        .snap
        .jobs
        .iter()
        .filter(|j| j.state == JobState::Queued)
        .count();
    let active = app.snap.jobs.iter().filter(|j| j.state.is_active()).count();
    let failed_hist = app
        .snap
        .history
        .iter()
        .filter(|h| h.state == JobState::Failed)
        .count();

    let savings_hint = match calibrated {
        Some(n) => trf!("estimate, calibrated with {} jobs", n),
        None => tr!("estimate, not guaranteed").to_string(),
    };
    let optimized_hint = trf!("from {}", format::bytes(optimized_src));
    let tiles: [(&str, String, Option<&str>, egui::Color32); 8] = [
        (
            tr!("Videos found"),
            lib.len().to_string(),
            None,
            theme::TEXT_STRONG,
        ),
        (
            tr!("Total source size"),
            format::bytes(total_size),
            None,
            theme::TEXT_STRONG,
        ),
        (
            tr!("Already optimized"),
            format::bytes(optimized),
            Some(&optimized_hint),
            theme::OK,
        ),
        (
            tr!("Estimated savings"),
            format!("≈ {}", format::bytes(savings)),
            Some(&savings_hint),
            theme::ACCENT,
        ),
        (
            tr!("Waiting jobs"),
            waiting.to_string(),
            None,
            theme::TEXT_STRONG,
        ),
        (
            tr!("Active jobs"),
            active.to_string(),
            None,
            if active > 0 {
                theme::ACCENT
            } else {
                theme::TEXT_STRONG
            },
        ),
        (
            tr!("Successful jobs"),
            done_hist.to_string(),
            None,
            theme::OK,
        ),
        (
            tr!("Failed jobs"),
            failed_hist.to_string(),
            None,
            if failed_hist > 0 {
                theme::ERROR
            } else {
                theme::TEXT_STRONG
            },
        ),
    ];
    let gap = 12.0;
    let per_row = if ui.available_width() < 760.0 { 2 } else { 4 };
    let width = (ui.available_width() - gap * (per_row - 1) as f32) / per_row as f32;
    for row in tiles.chunks(per_row) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for (title, value, hint, color) in row {
                theme::stat_tile(ui, width, title, value.clone(), *hint, *color);
            }
        });
        ui.add_space(gap - 6.0);
    }

    ui.add_space(6.0);
    ui.columns(2, |cols| {
        let ui = &mut cols[0];
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::section_title(ui, tr!("Recommendations"));
            egui::Grid::new("recommendations")
                .num_columns(3)
                .spacing([16.0, 8.0])
                .show(ui, |ui| {
                    for rec in [
                        Recommendation::Transcode,
                        Recommendation::Remux,
                        Recommendation::AlreadyOptimal,
                        Recommendation::Review,
                    ] {
                        let (n, size) = lib
                            .iter()
                            .filter(|e| e.analysis.recommendation == rec)
                            .fold((0, 0u64), |(n, s), e| (n + 1, s + e.media.size_bytes));
                        ui.horizontal(|ui| {
                            let (dot, _) =
                                ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                            ui.painter()
                                .circle_filled(dot.center(), 4.0, rec_color(rec));
                            ui.label(rec.label());
                        });
                        ui.label(
                            RichText::new(n.to_string())
                                .strong()
                                .color(theme::TEXT_STRONG),
                        );
                        ui.label(RichText::new(format::bytes(size)).color(theme::MUTED));
                        ui.end_row();
                    }
                });
            if let Some(summary) = &app.snap.scan_summary
                && (!summary.issues.is_empty() || !summary.duplicates.is_empty())
            {
                ui.add_space(4.0);
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
            ui.set_width(ui.available_width());
            theme::section_title(ui, tr!("Largest files"));
            if lib.is_empty() {
                ui.label(
                    RichText::new(tr!("Scan the source to see your videos here"))
                        .color(theme::MUTED),
                );
                return;
            }
            let mut idx: Vec<usize> = (0..lib.len()).collect();
            idx.sort_by_key(|&i| std::cmp::Reverse(lib[i].media.size_bytes));
            egui::Grid::new("largest")
                .num_columns(3)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    for &i in idx.iter().take(10) {
                        let e = &lib[i];
                        ui.label(
                            RichText::new(format::bytes(e.media.size_bytes))
                                .monospace()
                                .color(theme::TEXT_STRONG),
                        );
                        let rec = e.analysis.recommendation;
                        theme::badge(ui, rec.label(), rec_color(rec));
                        // long release names are truncated; full path on hover
                        let link = ui
                            .add(
                                egui::Label::new(
                                    RichText::new(&e.media.file_name).color(theme::TEXT),
                                )
                                .truncate()
                                .sense(egui::Sense::click()),
                            )
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .on_hover_text(e.media.relative_path.display().to_string());
                        if link.clicked() {
                            app.library_ui.focus = Some(e.media.absolute_path.clone());
                            app.view = View::Library;
                        }
                        ui.end_row();
                    }
                });
        });
    });
}
