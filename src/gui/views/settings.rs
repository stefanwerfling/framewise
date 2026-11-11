//! Settings: directories, tools, language, scan, safety, resources, hardware.

use std::path::PathBuf;
use std::sync::mpsc;

use eframe::egui::{self, RichText};

use crate::core::config::{AppConfig, HwBackend};
use crate::core::paths::{self, ConflictStrategy};
use crate::gui::{FramewiseApp, theme};
use crate::i18n::Language;
use crate::{tr, trf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathField {
    Source,
    Target,
    Work,
}

/// Cached validation result for a (source, target) pair.
type RootsCheck = ((Option<PathBuf>, Option<PathBuf>), Result<(), String>);

pub struct SettingsUi {
    draft: AppConfig,
    picker: Option<(PathField, mpsc::Receiver<Option<PathBuf>>)>,
    roots_check: Option<RootsCheck>,
}

impl SettingsUi {
    pub fn new(config: &AppConfig) -> Self {
        Self {
            draft: config.clone(),
            picker: None,
            roots_check: None,
        }
    }

    /// Native folder dialog on a separate thread (never blocks the GUI).
    fn pick(&mut self, field: PathField, start: Option<PathBuf>) {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut dialog = rfd::FileDialog::new();
            if let Some(dir) = start.filter(|d| d.is_dir()) {
                dialog = dialog.set_directory(dir);
            }
            let _ = tx.send(dialog.pick_folder());
        });
        self.picker = Some((field, rx));
    }

    fn poll_picker(&mut self) {
        let Some((field, rx)) = &self.picker else {
            return;
        };
        match rx.try_recv() {
            Ok(result) => {
                if let Some(p) = result {
                    match field {
                        PathField::Source => self.draft.source_dir = Some(p),
                        PathField::Target => self.draft.target_dir = Some(p),
                        PathField::Work => self.draft.work_dir = Some(p),
                    }
                }
                self.picker = None;
            }
            Err(mpsc::TryRecvError::Disconnected) => self.picker = None,
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }

    fn roots_status(&mut self) -> Option<Result<(), String>> {
        let key = (self.draft.source_dir.clone(), self.draft.target_dir.clone());
        if let (Some(s), Some(t)) = &key {
            if self.roots_check.as_ref().is_none_or(|(k, _)| *k != key) {
                let result = paths::validate_roots(s, t)
                    .map(|_| ())
                    .map_err(|e| e.to_string());
                self.roots_check = Some((key, result));
            }
            self.roots_check.as_ref().map(|(_, r)| r.clone())
        } else {
            None
        }
    }
}

fn path_row(ui: &mut egui::Ui, label: &str, value: &mut Option<PathBuf>, hint: &str) -> bool {
    ui.label(label);
    let mut text = value
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let mut browse = false;
    ui.horizontal(|ui| {
        if ui
            .add(
                egui::TextEdit::singleline(&mut text)
                    .hint_text(hint)
                    .desired_width(420.0),
            )
            .changed()
        {
            *value = (!text.trim().is_empty()).then(|| PathBuf::from(text.trim()));
        }
        browse = ui.button(tr!("Browse…")).clicked();
    });
    ui.end_row();
    browse
}

fn backend_combo(ui: &mut egui::Ui, id: &str, value: &mut HwBackend) {
    egui::ComboBox::from_id_salt(id)
        .selected_text(value.label())
        .show_ui(ui, |ui| {
            for b in HwBackend::ALL {
                ui.selectable_value(value, b, b.label());
            }
        });
}

pub fn show(app: &mut FramewiseApp, ui: &mut egui::Ui) {
    let s = &mut app.settings_ui;
    s.poll_picker();
    let current = app.snap.config.clone();
    // Keep fields managed elsewhere in sync.
    s.draft.encoding.active_profile = current.encoding.active_profile.clone();
    let dirty = s.draft != current;

    ui.horizontal(|ui| {
        ui.heading(tr!("Settings"));
        if dirty {
            ui.label(RichText::new(tr!("unsaved changes")).color(theme::WARN));
        }
    });
    let mut save = false;
    let mut revert = false;
    ui.horizontal(|ui| {
        save = ui
            .add_enabled(
                dirty,
                egui::Button::new(RichText::new(tr!("Save")).strong()),
            )
            .clicked();
        revert = ui
            .add_enabled(dirty, egui::Button::new(tr!("Discard")))
            .clicked();
    });
    ui.separator();

    let mut pick: Option<(PathField, Option<PathBuf>)> = None;
    egui::ScrollArea::vertical().show(ui, |ui| {
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(tr!("Directories")).strong());
            egui::Grid::new("dirs").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                if path_row(ui, tr!("Source directory"), &mut s.draft.source_dir, "/media/movies") {
                    pick = Some((PathField::Source, s.draft.source_dir.clone()));
                }
                if path_row(ui, tr!("Target directory"), &mut s.draft.target_dir, "/media/optimized") {
                    pick = Some((PathField::Target, s.draft.target_dir.clone()));
                }
                if path_row(ui, tr!("Work directory"), &mut s.draft.work_dir, tr!("default: .framewise-work in the target directory")) {
                    pick = Some((PathField::Work, s.draft.work_dir.clone()));
                }
            });
            match s.roots_status() {
                Some(Ok(())) => {
                    ui.label(RichText::new(tr!("✔ Source and target are valid")).color(theme::OK));
                }
                Some(Err(e)) => {
                    ui.label(RichText::new(format!("✖ {e}")).color(theme::ERROR));
                }
                None => {}
            }
            ui.label(RichText::new(tr!("The folder structure of the source is recreated in the target. Originals are never modified; temporary files are only written to the work directory (ideally on the target drive for fast moves).")).small().color(theme::MUTED));
        });

        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(tr!("Interface")).strong());
            egui::Grid::new("ui").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                ui.label(tr!("Language"));
                let label = s.draft.ui.language.map_or(tr!("System"), Language::native_name);
                egui::ComboBox::from_id_salt("lang").selected_text(label).show_ui(ui, |ui| {
                    ui.selectable_value(&mut s.draft.ui.language, None, tr!("System"));
                    for l in Language::ALL {
                        ui.selectable_value(&mut s.draft.ui.language, Some(l), l.native_name());
                    }
                });
                ui.end_row();
                ui.label(tr!("Scale"));
                ui.add(egui::Slider::new(&mut s.draft.ui.scale, 0.75..=2.0).step_by(0.05));
                ui.end_row();
                ui.label(tr!("Notifications"));
                ui.checkbox(
                    &mut s.draft.ui.desktop_notifications,
                    tr!("Desktop notification when a job or the queue finishes"),
                );
                ui.end_row();
            });
        });

        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new("FFmpeg").strong());
            egui::Grid::new("tools").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                ui.label("ffmpeg");
                let mut t = s.draft.ffmpeg_path.display().to_string();
                if ui.text_edit_singleline(&mut t).changed() {
                    s.draft.ffmpeg_path = PathBuf::from(t);
                }
                ui.end_row();
                ui.label("ffprobe");
                let mut t = s.draft.ffprobe_path.display().to_string();
                if ui.text_edit_singleline(&mut t).changed() {
                    s.draft.ffprobe_path = PathBuf::from(t);
                }
                ui.end_row();
            });
        });

        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(tr!("Scanner")).strong());
            egui::Grid::new("scan").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                ui.label(tr!("File extensions"));
                let mut ext = s.draft.scan.extensions.join(", ");
                if ui.text_edit_singleline(&mut ext).changed() {
                    s.draft.scan.extensions = ext
                        .split(',')
                        .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
                        .filter(|e| !e.is_empty())
                        .collect();
                }
                ui.end_row();
                ui.label(tr!("Symbolic links"));
                ui.checkbox(&mut s.draft.scan.follow_symlinks, tr!("follow (loops are detected)"));
                ui.end_row();
                ui.label(tr!("Hidden files"));
                ui.checkbox(&mut s.draft.scan.skip_hidden, tr!("skip"));
                ui.end_row();
                ui.label(tr!("Parallel analyses"));
                ui.add(egui::DragValue::new(&mut s.draft.scan.probe_parallelism).range(0..=32))
                    .on_hover_text(tr!("0 = automatic"));
                ui.end_row();
                ui.label(tr!("Duplicates"));
                ui.checkbox(&mut s.draft.scan.detect_duplicates, tr!("detect"));
                ui.end_row();
                ui.label(tr!("Largest N per folder"));
                ui.add(egui::DragValue::new(&mut s.draft.scan.top_n_per_folder).range(1..=50));
                ui.end_row();
            });
        });

        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(tr!("Output and safety")).strong());
            egui::Grid::new("safety").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                ui.label(tr!("If target exists"));
                egui::ComboBox::from_id_salt("conflict")
                    .selected_text(s.draft.encoding.conflict_strategy.label())
                    .show_ui(ui, |ui| {
                        for c in ConflictStrategy::ALL {
                            ui.selectable_value(&mut s.draft.encoding.conflict_strategy, c, c.label());
                        }
                    });
                ui.end_row();
                ui.label(tr!("Full decode test"));
                ui.checkbox(&mut s.draft.encoding.full_decode_test, tr!("decode every result completely (slower, safer)"));
                ui.end_row();
                ui.label(tr!("Failed results"));
                ui.checkbox(&mut s.draft.encoding.quarantine_failed, tr!("keep in quarantine folder for analysis"));
                ui.end_row();
            });
        });

        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(tr!("Resources")).strong());
            egui::Grid::new("res").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                let r = &mut s.draft.resources;
                ui.label(tr!("Parallel jobs"));
                ui.add(egui::DragValue::new(&mut r.max_parallel_jobs).range(1..=16));
                ui.end_row();
                ui.label(tr!("Encoder threads"));
                ui.add(egui::DragValue::new(&mut r.cpu_threads).range(0..=256)).on_hover_text(tr!("0 = automatic"));
                ui.end_row();
                ui.label(tr!("CPU target"));
                ui.add(egui::Slider::new(&mut r.cpu_target_percent, 10.0..=100.0).suffix(" %"))
                    .on_hover_text(tr!("No new job starts while the CPU load is above this value"));
                ui.end_row();
                ui.label(tr!("Min. free RAM"));
                ui.add(egui::DragValue::new(&mut r.min_free_ram_mb).range(0..=65536).suffix(" MB"));
                ui.end_row();
                ui.label(tr!("Min. free disk space"));
                ui.add(egui::DragValue::new(&mut r.min_free_disk_gb).range(0..=10000).suffix(" GB"));
                ui.end_row();
                ui.label(tr!("Process priority (nice)"));
                ui.add(egui::Slider::new(&mut r.nice_level, 0..=19))
                    .on_hover_text(tr!("Higher = lower priority for ffmpeg (Unix); Windows uses 'below normal' for values > 0"));
                ui.end_row();
            });
        });

        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(tr!("Hardware acceleration")).strong());
            egui::Grid::new("hw").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                let r = &mut s.draft.resources;
                ui.label(tr!("Hardware encoding"));
                ui.horizontal(|ui| {
                    ui.checkbox(&mut r.hw_encode_enabled, tr!("enable"));
                    ui.add_enabled_ui(r.hw_encode_enabled, |ui| backend_combo(ui, "hw_enc", &mut r.hw_encode));
                });
                ui.end_row();
                ui.label(tr!("Hardware decoding"));
                ui.horizontal(|ui| {
                    ui.checkbox(&mut r.hw_decode_enabled, tr!("enable"));
                    ui.add_enabled_ui(r.hw_decode_enabled, |ui| backend_combo(ui, "hw_dec", &mut r.hw_decode));
                });
                ui.end_row();
                ui.label(tr!("Fallback"));
                ui.checkbox(&mut r.fallback_to_cpu, tr!("retry with CPU if hardware fails"));
                ui.end_row();
                ui.label(tr!("VAAPI device"));
                ui.text_edit_singleline(&mut r.vaapi_device);
                ui.end_row();
                ui.label(tr!("NVIDIA GPU index"));
                ui.add(egui::DragValue::new(&mut r.gpu_index).range(0..=15));
                ui.end_row();
            });
            if let Some(caps) = &app.snap.caps {
                let working: Vec<&str> = caps.tests.iter().filter(|t| t.works).map(|t| t.encoder.as_str()).collect();
                ui.label(
                    RichText::new(if working.is_empty() {
                        tr!("No working hardware encoder found – CPU encoding (libx264) is used.").to_string()
                    } else {
                        trf!("Verified: {}", working.join(", "))
                    })
                    .small()
                    .color(theme::MUTED),
                );
            }
            ui.label(RichText::new(tr!("Hardware encoders usually produce larger files at the same quality than libx264. The application is fully usable without a GPU.")).small().color(theme::MUTED));
        });
    });

    if let Some((field, start)) = pick {
        s.pick(field, start);
    }
    if revert {
        s.draft = current;
    } else if save {
        let mut cfg = s.draft.clone();
        cfg.encoding.active_profile = app.snap.config.encoding.active_profile.clone();
        app.update_config(|c| *c = cfg);
    }
}
