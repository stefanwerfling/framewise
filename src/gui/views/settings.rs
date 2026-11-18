//! Settings: directories, tools, language, scan, safety, resources, hardware.

use std::path::PathBuf;
use std::sync::mpsc;

use eframe::egui::{self, RichText};

use crate::core::config::{AppConfig, HwBackend, SourceKind};
use crate::core::events::Command;
use crate::core::paths::{self, ConflictStrategy};
use crate::gui::{FramewiseApp, theme};
use crate::i18n::Language;
use crate::provider::jellyfin::{self, QuickConnectEvent, ServerInfo};
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
    /// Last connection test (`None` while none was made or one is running).
    jellyfin: Option<Result<ServerInfo, String>>,
    jellyfin_testing: bool,
    show_api_key: bool,
    quick_connect: QuickConnectUi,
    /// Test the connection on the next frame (after a login).
    test_pending: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum QuickConnectUi {
    #[default]
    Idle,
    Starting,
    Code(String),
    Failed(String),
}

/// What the Jellyfin card asks the backend to do.
enum JellyfinAction {
    Test,
    QuickConnect,
    CancelQuickConnect,
}

impl SettingsUi {
    pub fn new(config: &AppConfig) -> Self {
        Self {
            draft: config.clone(),
            picker: None,
            roots_check: None,
            jellyfin: None,
            jellyfin_testing: false,
            show_api_key: false,
            quick_connect: QuickConnectUi::Idle,
            test_pending: false,
        }
    }

    /// The language is switched (and saved) immediately, so the draft
    /// follows instead of showing it as an unsaved change.
    pub fn on_language_changed(&mut self, lang: Language) {
        self.draft.ui.language = Some(lang);
    }

    pub fn on_quick_connect(&mut self, event: QuickConnectEvent) {
        match event {
            QuickConnectEvent::Code(code) => self.quick_connect = QuickConnectUi::Code(code),
            QuickConnectEvent::Done(Ok(login)) => {
                self.quick_connect = QuickConnectUi::Idle;
                let j = &mut self.draft.jellyfin;
                j.api_key = login.token;
                j.user_id = login.user_id;
                j.user_name = login.user_name;
                self.show_api_key = false;
                self.test_pending = true;
            }
            QuickConnectEvent::Done(Err(e)) => {
                // A cancel the user asked for needs no message.
                self.quick_connect = if self.quick_connect == QuickConnectUi::Idle {
                    QuickConnectUi::Idle
                } else {
                    QuickConnectUi::Failed(e)
                };
            }
        }
    }

    pub fn on_jellyfin(&mut self, result: Result<ServerInfo, String>) {
        self.jellyfin_testing = false;
        self.jellyfin = Some(result);
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

    let mut save = false;
    let mut revert = false;
    theme::page_header(
        ui,
        tr!("Settings"),
        Some(tr!("Sources, tools, resources and interface")),
        |ui| {
            save = if dirty {
                ui.add(theme::primary_button(tr!("Save"))).clicked()
            } else {
                ui.add_enabled(false, egui::Button::new(tr!("Save")))
                    .clicked()
            };
            revert = ui
                .add_enabled(dirty, egui::Button::new(tr!("Discard")))
                .clicked();
            if dirty {
                theme::badge(ui, tr!("unsaved changes"), theme::WARN);
            }
        },
    );

    let mut language: Option<Language> = None;
    let mut pick: Option<(PathField, Option<PathBuf>)> = None;
    let mut jellyfin_action = None;
    egui::ScrollArea::vertical().show(ui, |ui| {
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::section_title(ui, tr!("Source"));
            ui.horizontal(|ui| {
                for kind in SourceKind::ALL {
                    ui.radio_value(&mut s.draft.source, kind, kind.label());
                }
            });
            if s.draft.source == SourceKind::Jellyfin {
                ui.label(RichText::new(tr!("Report only: framewise reads what the server knows about each file and rates it against the active profile. The files themselves are not accessed, so these items cannot be added to the queue.")).small().color(theme::MUTED));
            }
        });

        if s.draft.source == SourceKind::Jellyfin {
            jellyfin_action = jellyfin_card(ui, s);
        }

        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::section_title(ui, tr!("Directories"));
            if s.draft.source == SourceKind::Jellyfin {
                ui.label(RichText::new(tr!("Only used for local files.")).small().color(theme::MUTED));
            }
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

        health_card(ui, &mut s.draft.health);

        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::section_title(ui, tr!("Interface"));
            egui::Grid::new("ui").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                ui.label(tr!("Language"));
                let current = s.draft.ui.language.unwrap_or_default();
                egui::ComboBox::from_id_salt("lang")
                    .selected_text(current.native_name())
                    .show_ui(ui, |ui| {
                        for l in Language::ALL {
                            if ui.selectable_label(l == current, l.native_name()).clicked()
                                && l != current
                            {
                                language = Some(l);
                            }
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
            theme::section_title(ui, "FFmpeg");
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
            theme::section_title(ui, tr!("Scanner"));
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
            theme::section_title(ui, tr!("Output and safety"));
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
            theme::section_title(ui, tr!("Resources"));
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
            theme::section_title(ui, tr!("Hardware acceleration"));
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
    if std::mem::take(&mut s.test_pending) && s.draft.jellyfin.is_configured() {
        jellyfin_action = Some(JellyfinAction::Test);
    }
    let command = match jellyfin_action {
        Some(JellyfinAction::Test) => {
            s.jellyfin_testing = true;
            s.jellyfin = None;
            Some(Command::TestJellyfin(s.draft.jellyfin.clone()))
        }
        Some(JellyfinAction::QuickConnect) => {
            s.quick_connect = QuickConnectUi::Starting;
            if s.draft.jellyfin.device_id.is_empty() {
                s.draft.jellyfin.device_id = jellyfin::new_device_id();
            }
            Some(Command::QuickConnect {
                url: s.draft.jellyfin.url.clone(),
                device_id: s.draft.jellyfin.device_id.clone(),
            })
        }
        Some(JellyfinAction::CancelQuickConnect) => {
            s.quick_connect = QuickConnectUi::Idle;
            Some(Command::CancelQuickConnect)
        }
        None => None,
    };
    if revert {
        s.draft = current;
    } else if save {
        let mut cfg = s.draft.clone();
        cfg.encoding.active_profile = app.snap.config.encoding.active_profile.clone();
        app.update_config(|c| *c = cfg);
    }
    if let Some(command) = command {
        app.send(command);
    }
    if let Some(lang) = language {
        app.set_language(lang);
    }
}

/// Server address, login, connection test and library choice.
fn jellyfin_card(ui: &mut egui::Ui, s: &mut SettingsUi) -> Option<JellyfinAction> {
    let mut action = None;
    theme::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        theme::section_title(ui, "Jellyfin");
        let j = &mut s.draft.jellyfin;
        egui::Grid::new("jellyfin").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label(tr!("Server address"));
            ui.add(
                egui::TextEdit::singleline(&mut j.url)
                    .hint_text("http://jellyfin.local:8096")
                    .desired_width(420.0),
            );
            ui.end_row();
            ui.label(tr!("Login"));
            ui.vertical(|ui| {
                if j.user_id.is_empty() {
                    ui.horizontal(|ui| {
                        let edit = ui.add(
                            egui::TextEdit::singleline(&mut j.api_key)
                                .password(!s.show_api_key)
                                .hint_text(tr!("API key"))
                                .desired_width(320.0),
                        );
                        if edit.changed() {
                            s.jellyfin = None;
                        }
                        ui.checkbox(&mut s.show_api_key, tr!("show"));
                    });
                } else {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(trf!("✔ Logged in as {} (Quick Connect)", j.user_name))
                                .color(theme::OK),
                        );
                        if ui.small_button(tr!("Log out")).clicked() {
                            j.api_key.clear();
                            j.user_id.clear();
                            j.user_name.clear();
                            s.jellyfin = None;
                        }
                    });
                }
                ui.horizontal(|ui| match &s.quick_connect {
                    QuickConnectUi::Idle | QuickConnectUi::Failed(_) => {
                        let ready = !j.url.trim().is_empty();
                        if ui
                            .add_enabled(ready, egui::Button::new(tr!("Log in with Quick Connect")))
                            .on_disabled_hover_text(tr!("Enter the server address first"))
                            .clicked()
                        {
                            action = Some(JellyfinAction::QuickConnect);
                        }
                        if let QuickConnectUi::Failed(e) = &s.quick_connect {
                            ui.label(RichText::new(format!("✖ {e}")).color(theme::ERROR));
                        }
                    }
                    QuickConnectUi::Starting => {
                        ui.spinner();
                        ui.label(tr!("Requesting a code …"));
                        if ui.small_button(tr!("Cancel")).clicked() {
                            action = Some(JellyfinAction::CancelQuickConnect);
                        }
                    }
                    QuickConnectUi::Code(code) => {
                        ui.label(RichText::new(code).monospace().size(22.0).strong());
                        ui.spinner();
                        if ui.small_button(tr!("Cancel")).clicked() {
                            action = Some(JellyfinAction::CancelQuickConnect);
                        }
                    }
                });
                if let QuickConnectUi::Code(_) = &s.quick_connect {
                    ui.label(
                        RichText::new(tr!("Enter this code in Jellyfin: user menu → Quick Connect (on any device where you are logged in)."))
                            .small(),
                    );
                }
            });
            ui.end_row();
        });
        ui.label(
            RichText::new(tr!("Use an API key (Dashboard → API keys) or log in with Quick Connect. Without administrator rights only your own libraries are listed. Key and login are stored in the configuration file, readable only by your user."))
                .small()
                .color(theme::MUTED),
        );
        ui.horizontal(|ui| {
            let ready = j.is_configured() && !s.jellyfin_testing;
            if ui
                .add_enabled(ready, egui::Button::new(tr!("Test connection")))
                .clicked()
            {
                action = Some(JellyfinAction::Test);
            }
            if s.jellyfin_testing {
                ui.spinner();
            }
            match &s.jellyfin {
                Some(Ok(info)) => {
                    ui.label(
                        RichText::new(trf!("✔ {} (Jellyfin {})", info.name, info.version))
                            .color(theme::OK),
                    );
                }
                Some(Err(e)) => {
                    ui.label(RichText::new(format!("✖ {e}")).color(theme::ERROR));
                }
                None => {}
            }
        });

        let Some(Ok(info)) = &s.jellyfin else {
            if !j.libraries.is_empty() {
                ui.label(
                    RichText::new(trf!("{} libraries selected – test the connection to change the selection", j.libraries.len()))
                        .small()
                        .color(theme::MUTED),
                );
            }
            return;
        };
        ui.add_space(4.0);
        ui.label(tr!("Libraries"));
        let chosen: Vec<String> = jellyfin::selected(j, &info.libraries)
            .into_iter()
            .map(|l| l.id.clone())
            .collect();
        for lib in &info.libraries {
            let mut on = chosen.contains(&lib.id);
            let kind = lib.collection_type.as_deref().unwrap_or("mixed");
            let label = format!("{}  ({kind})", lib.name);
            if ui
                .add_enabled(lib.is_video(), egui::Checkbox::new(&mut on, label))
                .on_disabled_hover_text(tr!("Contains no movies or series"))
                .changed()
            {
                let mut ids = chosen.clone();
                if on {
                    ids.push(lib.id.clone());
                } else {
                    ids.retain(|id| id != &lib.id);
                }
                // An empty list means "all"; the last library stays selected.
                if !ids.is_empty() {
                    j.libraries = ids;
                }
            }
        }
        if !j.libraries.is_empty() && ui.small_button(tr!("All movie and series libraries")).clicked() {
            j.libraries.clear();
        }
    });
    action
}

/// Yardstick of the playback risk (fixed) and thoroughness of the quick check.
fn health_card(ui: &mut egui::Ui, h: &mut crate::core::config::HealthSettings) {
    theme::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        theme::section_title(ui, tr!("Health check"));
        ui.label(tr!("Playback risk measures against the most compatible format – what every common TV player plays directly, the plain 1080p Fire TV stick included, so the server only has to deliver the file:"));
        ui.label(
            RichText::new(tr!("MP4/MKV · H.264 (8 bit) or HEVC · up to 1080p · at most 20 Mbit/s · progressive, constant frame rate, closed GOPs · SDR/HDR10/HLG, Dolby Vision only with HDR10 base · audio AAC/AC3/E-AC3 · text subtitles · MP4 index at the start"))
                .small()
                .color(theme::MUTED),
        );
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(tr!("Quick check"));
            ui.add(egui::DragValue::new(&mut h.samples).range(1..=20));
            ui.label(tr!("samples of"));
            ui.add(egui::DragValue::new(&mut h.sample_seconds).range(1..=30).suffix(" s"));
            ui.label(tr!("· files at once"));
            ui.add(egui::DragValue::new(&mut h.parallel).range(1..=8));
        });
        ui.label(
            RichText::new(tr!("More and longer samples find more but transfer more data: per file roughly samples × length × bitrate."))
                .small()
                .color(theme::MUTED),
        );
    });
}
