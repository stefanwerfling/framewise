//! History of finished jobs.

use eframe::egui::{self, RichText};
use egui_extras::{Column, TableBuilder};

use crate::gui::{FramewiseApp, format, theme};
use crate::queue::job::JobState;
use crate::{tr, trf};

#[derive(Default)]
pub struct HistoryUi {
    pub search: String,
    pub only_failed: bool,
}

pub fn show(app: &mut FramewiseApp, ui: &mut egui::Ui) {
    ui.heading(tr!("History"));
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
    ui.separator();

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

    let profiles = &app.snap.profiles;
    TableBuilder::new(ui)
        .striped(true)
        .resizable(true)
        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
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
                        ui.label(RichText::new(tr!("passed")).color(theme::OK));
                    } else {
                        let text = h
                            .error
                            .clone()
                            .unwrap_or_else(|| h.state.label().to_string());
                        ui.label(RichText::new(&text).color(theme::ERROR).small())
                            .on_hover_text(text);
                    }
                });
            });
        });
}
