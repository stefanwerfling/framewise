//! History of finished jobs.

use eframe::egui::{self, RichText};
use egui_extras::{Column, TableBuilder};

use std::collections::HashSet;

use crate::core::events::{Command, HistoryRequeue};
use crate::gui::{FramewiseApp, format, theme};
use crate::queue::job::{HistoryEntry, JobId, JobState};
use crate::{tr, trf};

#[derive(Default)]
pub struct HistoryUi {
    pub search: String,
    pub only_failed: bool,
}

pub fn show(app: &mut FramewiseApp, ui: &mut egui::Ui) {
    theme::page_header(
        ui,
        tr!("History"),
        Some(tr!("Finished, failed and cancelled jobs")),
        |_| {},
    );
    let done: Vec<_> = app
        .snap
        .history
        .iter()
        .filter(|h| h.state == JobState::Done)
        .collect();
    let src: u64 = done.iter().map(|h| h.source_size).sum();
    let res: u64 = done.iter().filter_map(|h| h.result_size).sum();
    ui.horizontal(|ui| {
        ui.label(trf!(
            "{} successful · {} → {} (saved {})",
            done.len(),
            format::bytes(src),
            format::bytes(res),
            format::bytes(src.saturating_sub(res))
        ));
        ui.separator();
        ui.add(
            egui::TextEdit::singleline(&mut app.history_ui.search)
                .hint_text(tr!("Search…"))
                .desired_width(200.0),
        );
        ui.checkbox(&mut app.history_ui.only_failed, tr!("Only failed"));
    });

    let needle = app.history_ui.search.to_lowercase();
    let rows: Vec<_> = app
        .snap
        .history
        .iter()
        .filter(|h| !app.history_ui.only_failed || h.state != JobState::Done)
        .filter(|h| {
            needle.is_empty() || h.source.to_string_lossy().to_lowercase().contains(&needle)
        })
        .collect();

    // Failures that can go back on the queue: not done since, and the job is
    // not waiting or running already.
    let busy: HashSet<JobId> = app
        .snap
        .jobs
        .iter()
        .filter(|j| !j.state.can_retry())
        .map(|j| j.id)
        .collect();
    let history = &app.snap.history;
    let requeueable = |h: &HistoryEntry| {
        h.state != JobState::Done
            && !busy.contains(&h.job_id)
            && !history.iter().any(|o| {
                o.job_id == h.job_id
                    && o.state == JobState::Done
                    && o.finished_unix >= h.finished_unix
            })
    };
    let to_request = |h: &HistoryEntry| HistoryRequeue {
        job_id: h.job_id,
        source: h.source.clone(),
        target: h.target.clone(),
        profile_id: h.profile_id.clone(),
    };
    let mut seen = HashSet::new();
    let shown_failed: Vec<HistoryRequeue> = rows
        .iter()
        .filter(|h| requeueable(h) && seen.insert(h.job_id))
        .map(|h| to_request(h))
        .collect();
    let mut requeue: Option<Vec<HistoryRequeue>> = None;
    if !shown_failed.is_empty() {
        ui.horizontal(|ui| {
            if ui
                .button(trf!("↻ Requeue shown failures ({})", shown_failed.len()))
                .on_hover_text(tr!(
                    "Put the failed entries shown below back on the queue – they start from the beginning"
                ))
                .clicked()
            {
                requeue = Some(shown_failed.clone());
            }
        });
    }
    ui.separator();

    let profiles = &app.snap.profiles;
    TableBuilder::new(ui)
        .striped(true)
        .resizable(true)
        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
        .column(Column::exact(24.0))
        .column(Column::initial(120.0))
        .column(Column::initial(260.0).clip(true))
        .column(Column::initial(80.0))
        .column(Column::initial(80.0))
        .column(Column::initial(70.0))
        .column(Column::initial(110.0).clip(true))
        .column(Column::initial(110.0).clip(true))
        .column(Column::initial(70.0))
        .column(Column::initial(130.0))
        .column(Column::remainder().at_least(100.0))
        .header(22.0, |mut h| {
            h.col(|_| {});
            for t in [
                tr!("Finished"),
                tr!("File"),
                tr!("Original"),
                tr!("Result"),
                tr!("Savings"),
                tr!("Profile"),
                tr!("Encoder"),
                tr!("Duration"),
                tr!("Quality"),
                tr!("Validation"),
            ] {
                h.col(|ui| {
                    ui.strong(t);
                });
            }
        })
        .body(|body| {
            body.rows(22.0, rows.len(), |mut row| {
                let h = rows[row.index()];
                row.col(|ui| {
                    if requeueable(h)
                        && ui
                            .small_button("↻")
                            .on_hover_text(tr!("Put back on the queue"))
                            .clicked()
                    {
                        requeue = Some(vec![to_request(h)]);
                    }
                });
                row.col(|ui| {
                    ui.label(RichText::new(format::timestamp(h.finished_unix)).small());
                });
                row.col(|ui| {
                    let name = h
                        .source
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    ui.label(name).on_hover_text(format!(
                        "{}\n→ {}",
                        h.source.display(),
                        h.target.display()
                    ));
                });
                row.col(|ui| {
                    ui.label(format::bytes(h.source_size));
                });
                row.col(|ui| {
                    ui.label(format::opt_bytes(h.result_size));
                });
                row.col(|ui| match h.savings_percent() {
                    Some(p) => {
                        ui.label(RichText::new(format!("{p:.1} %")).color(if p > 0.0 {
                            theme::OK
                        } else {
                            theme::WARN
                        }));
                    }
                    None => {
                        ui.label("–");
                    }
                });
                row.col(|ui| {
                    let name = profiles
                        .iter()
                        .find(|p| p.id == h.profile_id)
                        .map_or(h.profile_id.as_str(), |p| p.name.as_str());
                    ui.label(name);
                });
                row.col(|ui| {
                    ui.label(RichText::new(h.encoder.clone().unwrap_or_default()).small());
                });
                row.col(|ui| {
                    ui.label(format::duration(h.encode_seconds.map(|s| s as f64)));
                });
                row.col(|ui| match &h.quality {
                    Some(q) => {
                        ui.label(format!(
                            "{} {:.3} · CRF {}",
                            q.metric.label(),
                            q.score,
                            q.crf
                        ))
                        .on_hover_text({
                            let mut text = trf!(
                                "min {}, threshold {}",
                                format!("{:.3}", q.min_score),
                                q.threshold
                            );
                            if let Some(a) = &q.auto {
                                for line in super::queue::auto_lines(a) {
                                    text.push('\n');
                                    text.push_str(&line);
                                }
                            }
                            text
                        });
                    }
                    None => {
                        ui.label("–");
                    }
                });
                row.col(|ui| {
                    if h.state == JobState::Done && h.validation_passed {
                        theme::badge(ui, tr!("passed"), theme::OK);
                    } else {
                        let text = h
                            .error
                            .clone()
                            .unwrap_or_else(|| h.state.label().to_string());
                        // cancelled or skipped is the user's choice, not an error
                        let color = if matches!(h.state, JobState::Cancelled | JobState::Skipped) {
                            theme::WARN
                        } else {
                            theme::ERROR
                        };
                        ui.label(RichText::new(&text).color(color).small())
                            .on_hover_text(text);
                    }
                });
            });
        });
    if let Some(items) = requeue {
        app.send(Command::RequeueHistory(items));
    }
}
