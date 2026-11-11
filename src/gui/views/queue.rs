//! Encoding queue with live progress.

use eframe::egui::{self, RichText};
use egui_extras::{Column, TableBuilder};

use crate::core::events::Command;
use crate::encoding::os::CAN_SUSPEND;
use crate::encoding::quality::{AutoChoice, AutoDecision};
use crate::gui::{FramewiseApp, format, theme};
use crate::queue::job::{self, Job, JobId, JobState};
use crate::{tr, trf};

#[derive(Default)]
pub struct QueueUi {
    pub focus: Option<JobId>,
    pub hide_finished: bool,
}

fn state_color(s: JobState) -> egui::Color32 {
    match s {
        JobState::Done => theme::OK,
        JobState::Failed => theme::ERROR,
        JobState::Cancelled | JobState::Skipped | JobState::Interrupted => theme::WARN,
        JobState::Queued => theme::MUTED,
        _ => theme::ACCENT,
    }
}

pub fn show(app: &mut FramewiseApp, ui: &mut egui::Ui) {
    ui.heading(tr!("Queue"));
    ui.horizontal_wrapped(|ui| {
        if app.snap.queue_paused {
            if ui
                .button(RichText::new(tr!("▶ Start / resume queue")).strong())
                .clicked()
            {
                app.send(Command::SetQueuePaused(false));
            }
            ui.label(
                RichText::new(tr!("Queue paused – no new jobs are started")).color(theme::WARN),
            );
        } else if ui.button(tr!("⏸ Pause queue")).clicked() {
            app.send(Command::SetQueuePaused(true));
        }
        if ui.button(tr!("Remove finished")).clicked() {
            app.send(Command::ClearFinished);
        }
        ui.checkbox(&mut app.queue_ui.hide_finished, tr!("Hide finished"));
        if let Some(gate) = &app.snap.gate {
            ui.label(
                RichText::new(trf!("Waiting for resources: {}", gate.describe()))
                    .color(theme::WARN),
            );
        }
        let r = &app.snap.config.resources;
        if let Some(eta) =
            job::queue_eta(&app.snap.jobs, &app.snap.history, r.max_parallel_jobs)
        {
            let mut text = trf!(
                "Queue remaining ≈ {} ({} jobs)",
                format::duration(Some(eta.seconds)),
                eta.jobs
            );
            if eta.incomplete {
                text.push_str(" +");
            }
            ui.label(RichText::new(text).strong()).on_hover_text(tr!(
                "Estimate from the current encoding speed (or past jobs); quality analysis and validation are not included"
            ));
        }
        ui.label(
            RichText::new(trf!(
                "Parallel jobs: {} · CPU target {} %",
                r.max_parallel_jobs,
                format!("{:.0}", r.cpu_target_percent)
            ))
            .color(theme::MUTED),
        );
    });
    ui.separator();

    let mut jobs: Vec<Job> = app
        .snap
        .jobs
        .iter()
        .filter(|j| !app.queue_ui.hide_finished || !j.state.is_finished())
        .cloned()
        .collect();
    // active first, then queued by priority/position, then finished (newest first)
    jobs.sort_by(|a, b| {
        let rank = |j: &Job| {
            if j.state.is_active() {
                0
            } else if matches!(j.state, JobState::Queued | JobState::Interrupted) {
                1
            } else {
                2
            }
        };
        rank(a)
            .cmp(&rank(b))
            .then(b.priority.cmp(&a.priority))
            .then_with(|| {
                if rank(a) == 2 {
                    b.finished_unix.cmp(&a.finished_unix)
                } else {
                    a.position.cmp(&b.position)
                }
            })
    });

    if let Some(job) = app
        .queue_ui
        .focus
        .and_then(|id| app.snap.jobs.iter().find(|j| j.id == id))
        .cloned()
    {
        egui::Panel::bottom("job_details")
            .resizable(true)
            .default_size(220.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| details(ui, &job));
            });
    }

    let mut commands: Vec<Command> = Vec::new();
    let mut focus = app.queue_ui.focus;
    let row_h = 26.0;
    // selectable labels would swallow the row click
    ui.style_mut().interaction.selectable_labels = false;
    // bound the table explicitly to the space left over by the details panel,
    // otherwise its column lines are drawn across the panel
    let table_rect = ui.available_rect_before_wrap();
    ui.scope_builder(egui::UiBuilder::new().max_rect(table_rect), |ui| {
    TableBuilder::new(ui)
        .striped(true)
        .resizable(true)
        .sense(egui::Sense::click())
        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
        .column(Column::initial(110.0))
        .column(Column::initial(260.0).clip(true))
        .column(Column::initial(200.0).at_least(120.0).clip(true))
        .column(Column::initial(70.0))
        .column(Column::initial(70.0))
        .column(Column::initial(70.0))
        .column(Column::initial(120.0).clip(true))
        .column(Column::initial(50.0))
        .column(Column::remainder().at_least(160.0))
        .header(22.0, |mut h| {
            for t in [
                tr!("Status"),
                tr!("Source → target"),
                tr!("Progress"),
                tr!("Speed"),
                tr!("Elapsed"),
                tr!("Remaining"),
                tr!("Mode"),
                tr!("Prio"),
                tr!("Actions"),
            ] {
                h.col(|ui| {
                    ui.strong(t);
                });
            }
        })
        .body(|body| {
            body.rows(row_h, jobs.len(), |mut row| {
                let j = &jobs[row.index()];
                row.set_selected(focus == Some(j.id));
                row.col(|ui| {
                    let mut text = j.state.label().to_string();
                    if j.paused {
                        text = format!("⏸ {text}");
                    }
                    ui.label(RichText::new(text).color(state_color(j.state)));
                });
                row.col(|ui| {
                    ui.label(&j.media.file_name).on_hover_text(format!(
                        "{}\n→ {}",
                        j.media.absolute_path.display(),
                        j.target.display()
                    ));
                });
                row.col(|ui| {
                    if j.state.is_active() {
                        let fraction = j.progress.fraction.unwrap_or(0.0);
                        let text = match (j.state, j.progress.probe_crf) {
                            (JobState::QualitySearch, Some(crf)) => trf!("test CRF {}", crf),
                            _ if j.progress.fraction.is_some() => {
                                format!("{:.1} %", fraction * 100.0)
                            }
                            _ => j.state.label().to_string(),
                        };
                        let mut bar = egui::ProgressBar::new(fraction).text(text);
                        if j.progress.fraction.is_none() {
                            bar = bar.animate(true);
                        }
                        ui.add(bar);
                    } else if j.state == JobState::Done {
                        let saved = j
                            .result_size
                            .filter(|_| j.media.size_bytes > 0)
                            .map(|r| (1.0 - r as f64 / j.media.size_bytes as f64) * 100.0);
                        let text = trf!(
                            "{} → {} ({})",
                            format::bytes(j.media.size_bytes),
                            format::opt_bytes(j.result_size),
                            saved.map_or_else(
                                || "–".to_string(),
                                |s| if s >= 0.0 {
                                    format!("−{s:.0} %")
                                } else {
                                    format!("+{:.0} %", -s)
                                }
                            )
                        );
                        if saved.is_some_and(|s| s < 0.0) {
                            ui.label(RichText::new(text).color(theme::WARN))
                                .on_hover_text(tr!(
                                    "The result is larger than the source – the original may be the better choice"
                                ));
                        } else {
                            ui.label(text);
                        }
                    } else if let Some(e) = &j.error {
                        ui.add(
                            egui::Label::new(RichText::new(e).small().color(theme::ERROR))
                                .truncate(),
                        )
                        .on_hover_text(e);
                    }
                });
                row.col(|ui| {
                    if let Some(s) = j.progress.speed {
                        ui.label(format!("{s:.2}×"));
                    } else if let Some(fps) = j.progress.fps {
                        ui.label(format!("{fps:.0} fps"));
                    }
                });
                row.col(|ui| {
                    ui.label(format::duration(j.elapsed_s().map(|s| s as f64)));
                });
                row.col(|ui| {
                    ui.label(format::duration(j.progress.eta_s));
                });
                row.col(|ui| {
                    let mut t = j.encoder.clone().unwrap_or_else(|| "–".into());
                    if j.used_fallback {
                        t = format!("{t} ({})", tr!("CPU fallback"));
                    }
                    ui.label(RichText::new(t).small());
                });
                row.col(|ui| {
                    ui.label(j.priority.to_string());
                });
                row.col(|ui| {
                    if j.state.is_active() {
                        if CAN_SUSPEND {
                            if j.paused {
                                if ui.small_button("▶").on_hover_text(tr!("Resume")).clicked() {
                                    commands.push(Command::ResumeJob(j.id));
                                }
                            } else if ui.small_button("⏸").on_hover_text(tr!("Pause")).clicked() {
                                commands.push(Command::PauseJob(j.id));
                            }
                        }
                        if ui.small_button("✖").on_hover_text(tr!("Cancel")).clicked() {
                            commands.push(Command::CancelJob(j.id));
                        }
                    } else {
                        if matches!(j.state, JobState::Queued | JobState::Interrupted) {
                            if ui
                                .small_button("↑")
                                .on_hover_text(tr!("Higher priority"))
                                .clicked()
                            {
                                commands.push(Command::SetJobPriority(j.id, j.priority + 1));
                            }
                            if ui
                                .small_button("↓")
                                .on_hover_text(tr!("Lower priority"))
                                .clicked()
                            {
                                commands.push(Command::SetJobPriority(j.id, j.priority - 1));
                            }
                            if ui.small_button("✖").on_hover_text(tr!("Cancel")).clicked() {
                                commands.push(Command::CancelJob(j.id));
                            }
                        }
                        if j.state.can_retry()
                            && ui.small_button("↻").on_hover_text(tr!("Retry")).clicked()
                        {
                            commands.push(Command::RetryJob(j.id));
                        }
                        if ui
                            .small_button("🗑")
                            .on_hover_text(tr!("Remove from queue"))
                            .clicked()
                        {
                            commands.push(Command::RemoveJob(j.id));
                        }
                    }
                });
                if row.response().clicked() {
                    focus = if focus == Some(j.id) {
                        None
                    } else {
                        Some(j.id)
                    };
                }
            });
        });
    });
    app.queue_ui.focus = focus;
    for c in commands {
        app.send(c);
    }
}

fn details(ui: &mut egui::Ui, j: &Job) {
    ui.horizontal(|ui| {
        ui.strong(&j.media.file_name);
        ui.label(RichText::new(j.state.label()).color(state_color(j.state)));
        ui.label(RichText::new(trf!("Attempt {}", j.attempts)).color(theme::MUTED));
    });
    ui.label(
        RichText::new(format!(
            "{} → {}",
            j.media.absolute_path.display(),
            j.target.display()
        ))
        .small(),
    );
    if j.replace_existing {
        ui.label(
            RichText::new(tr!(
                "Replaces the existing result after successful validation"
            ))
            .color(theme::WARN),
        );
    }
    if let Some(e) = &j.error {
        ui.label(RichText::new(e).color(theme::ERROR));
    }
    if let Some(log) = &j.log_file {
        ui.label(
            RichText::new(trf!("FFmpeg log: {}", log.display()))
                .small()
                .color(theme::MUTED),
        );
    }
    if !j.plan.warnings.is_empty() {
        ui.collapsing(trf!("{} notes", j.plan.warnings.len()), |ui| {
            for w in &j.plan.warnings {
                ui.label(RichText::new(format!("⚠ {}", w.describe())).small());
            }
        });
    }
    if let Some(q) = &j.quality {
        ui.label(trf!(
            "Quality: {} {} (min {}, threshold {}) at CRF {}",
            q.metric.label(),
            format!("{:.4}", q.score),
            format!("{:.4}", q.min_score),
            q.threshold,
            q.crf
        ));
        if !q.search.is_empty() {
            let steps = q
                .search
                .iter()
                .map(|(crf, score)| format!("CRF {crf}: {score:.2}"))
                .collect::<Vec<_>>()
                .join(" → ");
            ui.label(RichText::new(trf!("CRF search: {}", steps)).small());
        }
        if let Some(a) = &q.auto {
            for line in auto_lines(a) {
                ui.label(RichText::new(line).small());
            }
        }
        if q.vmaf_unavailable {
            ui.label(
                RichText::new(tr!("VMAF not available in this ffmpeg build – SSIM used"))
                    .small()
                    .color(theme::WARN),
            );
        }
    }
    if let Some(v) = &j.validation {
        let ok = v.passed();
        ui.label(
            RichText::new(if ok {
                tr!("Validation passed")
            } else {
                tr!("Validation failed")
            })
            .color(if ok { theme::OK } else { theme::ERROR }),
        );
        egui::Grid::new(("validation", j.id))
            .striped(true)
            .num_columns(4)
            .show(ui, |ui| {
                for c in &v.checks {
                    ui.label(if c.passed { "✔" } else { "✖" });
                    ui.label(c.kind.label());
                    ui.label(RichText::new(&c.expected).small());
                    ui.label(RichText::new(&c.actual).small());
                    ui.end_row();
                }
            });
    }
    for s in &j.sidecars {
        ui.label(RichText::new(trf!("Sidecar: {}", s.display())).small());
    }
}

/// Human-readable summary of the automatic method selection.
pub(crate) fn auto_lines(a: &AutoDecision) -> Vec<String> {
    let mut lines = Vec::new();
    let chosen = match a.choice {
        AutoChoice::Encode(codec) => {
            let preset = a
                .candidates
                .iter()
                .find(|c| c.codec == codec)
                .map(|c| c.preset.clone())
                .unwrap_or_default();
            trf!("Method: {} ({}) chosen", codec.label(), preset)
        }
        AutoChoice::CopyVideo => {
            tr!("Method: video stream copied – re-encoding would save too little").to_string()
        }
    };
    lines.push(chosen);
    for c in &a.candidates {
        let size = c
            .size_ratio
            .map(|r| trf!("≈ {} % of the source", format!("{:.0}", r * 100.0)))
            .unwrap_or_default();
        lines.push(format!(
            "{} {} {} · CRF {} · {} {:.2} / {:.2} · {} · {}",
            if c.passed { "✔" } else { "✘" },
            c.codec.label(),
            c.preset,
            c.crf,
            tr!("avg / weakest"),
            c.score,
            c.min_score,
            size,
            format::duration(Some(c.search_secs as f64)),
        ));
    }
    let times = a
        .window_starts
        .iter()
        .map(|s| format::duration(Some(*s)))
        .collect::<Vec<_>>()
        .join(", ");
    lines.push(if a.scene_windows {
        trf!("Samples at the hardest and typical scenes: {}", times)
    } else {
        trf!("Samples evenly spaced: {}", times)
    });
    lines.extend(a.notes.iter().cloned());
    lines
}
