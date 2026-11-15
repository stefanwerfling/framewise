//! Media library: sortable/filterable table with detail panel.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, mpsc};

use eframe::egui::{self, RichText};
use egui_extras::{Column, TableBuilder};

use super::dashboard::scan_controls;
use crate::analyzer::recommend::{Compatibility, Recommendation};
use crate::analyzer::report;
use crate::core::events::NoticeLevel;
use crate::core::events::{Command, LibraryEntry};
use crate::encoding::plan::{AudioAction, SubtitleAction, VideoAction};
use crate::gui::{FramewiseApp, format, selected_set, theme};
use crate::health::{HealthState, Severity};
use crate::media::model::{HdrFormat, SubtitleKind};
use crate::provider::summary::{self, Counts, LibrarySummary};
use crate::{tr, trf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SortKey {
    Name,
    Folder,
    Size,
    Resolution,
    Codec,
    Bitrate,
    Duration,
    Potential,
    Compatibility,
    Health,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HealthFilter {
    All,
    /// Playback risk, suspect or defect.
    Problems,
    Only(HealthState),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecFilter {
    All,
    Actionable,
    Only(Recommendation),
}

pub struct LibraryUi {
    pub search: String,
    pub rec_filter: RecFilter,
    pub health_filter: HealthFilter,
    /// Quick check also repeats files that were checked already.
    pub recheck: bool,
    pub only_hdr: bool,
    pub only_top: bool,
    pub only_duplicates: bool,
    pub hide_queued: bool,
    pub sort: SortKey,
    pub descending: bool,
    pub selected: HashSet<PathBuf>,
    pub focus: Option<PathBuf>,
    cache: Option<(usize, u64, Vec<usize>)>,
    /// Running CSV export: path chosen in the save dialog.
    export: Option<(String, mpsc::Receiver<Option<PathBuf>>)>,
    /// Series overview of the library behind the pointer.
    overview: Option<(usize, Arc<LibrarySummary>)>,
    /// Row index by path of the library behind the pointer.
    index: Option<(usize, Arc<HashMap<PathBuf, usize>>)>,
}

impl Default for LibraryUi {
    fn default() -> Self {
        Self {
            search: String::new(),
            rec_filter: RecFilter::All,
            health_filter: HealthFilter::All,
            recheck: false,
            only_hdr: false,
            only_top: false,
            only_duplicates: false,
            hide_queued: false,
            sort: SortKey::Size,
            descending: true,
            selected: HashSet::new(),
            focus: None,
            cache: None,
            export: None,
            overview: None,
            index: None,
        }
    }
}

impl LibraryUi {
    pub fn invalidate(&mut self) {
        self.cache = None;
    }

    /// Row by path (built once per library, the table can be large).
    fn index(&mut self, lib: &Arc<Vec<LibraryEntry>>) -> Arc<HashMap<PathBuf, usize>> {
        let ptr = Arc::as_ptr(lib) as usize;
        match &self.index {
            Some((p, map)) if *p == ptr => map.clone(),
            _ => {
                let map: Arc<HashMap<PathBuf, usize>> = Arc::new(
                    lib.iter()
                        .enumerate()
                        .map(|(i, e)| (e.media.absolute_path.clone(), i))
                        .collect(),
                );
                self.index = Some((ptr, map.clone()));
                map
            }
        }
    }

    fn filter_key(&self) -> u64 {
        let mut h = DefaultHasher::new();
        self.search.hash(&mut h);
        self.rec_filter.hash(&mut h);
        self.health_filter.hash(&mut h);
        (
            self.only_hdr,
            self.only_top,
            self.only_duplicates,
            self.hide_queued,
        )
            .hash(&mut h);
        self.sort.hash(&mut h);
        self.descending.hash(&mut h);
        h.finish()
    }

    /// Indices of visible rows (cached until library or filters change).
    fn visible(&mut self, lib: &Arc<Vec<LibraryEntry>>) -> Vec<usize> {
        let ptr = Arc::as_ptr(lib) as usize;
        let key = self.filter_key();
        if let Some((p, k, rows)) = &self.cache
            && *p == ptr
            && *k == key
        {
            return rows.clone();
        }
        let needle = self.search.to_lowercase();
        let mut rows: Vec<usize> = lib
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                (needle.is_empty()
                    || e.media
                        .relative_path
                        .to_string_lossy()
                        .to_lowercase()
                        .contains(&needle)
                    || e.media
                        .provider
                        .as_ref()
                        .is_some_and(|p| p.display_title().to_lowercase().contains(&needle)))
                    && match self.rec_filter {
                        RecFilter::All => true,
                        RecFilter::Actionable => matches!(
                            e.analysis.recommendation,
                            Recommendation::Transcode | Recommendation::Remux
                        ),
                        RecFilter::Only(r) => e.analysis.recommendation == r,
                    }
                    && match self.health_filter {
                        HealthFilter::All => true,
                        HealthFilter::Problems => e.health.state().is_problem(),
                        HealthFilter::Only(s) => e.health.state() == s,
                    }
                    && (!self.only_hdr || e.media.primary_video().is_some_and(|v| v.hdr.is_hdr()))
                    && (!self.only_top || e.top_in_folder || e.top_global)
                    && (!self.only_duplicates || e.duplicate)
                    && (!self.hide_queued || !e.queued)
            })
            .map(|(i, _)| i)
            .collect();
        // Keys that allocate are computed once per row, not per comparison
        // (tens of thousands of rows).
        match self.sort {
            SortKey::Name => {
                rows.sort_by_cached_key(|&i| lib[i].media.display_name().to_lowercase());
            }
            SortKey::Folder => rows.sort_by_cached_key(|&i| lib[i].media.folder()),
            SortKey::Codec => rows.sort_by_cached_key(|&i| codec(&lib[i])),
            SortKey::Health => rows.sort_by_cached_key(|&i| {
                let h = &lib[i].health;
                (h.state(), h.findings().len())
            }),
            _ => rows.sort_by(|&a, &b| {
                let (x, y) = (&lib[a], &lib[b]);
                match self.sort {
                    SortKey::Size => x.media.size_bytes.cmp(&y.media.size_bytes),
                    SortKey::Resolution => pixels(x).cmp(&pixels(y)),
                    SortKey::Bitrate => x.media.overall_bitrate().cmp(&y.media.overall_bitrate()),
                    SortKey::Duration => x
                        .media
                        .duration_s()
                        .unwrap_or(0.0)
                        .total_cmp(&y.media.duration_s().unwrap_or(0.0)),
                    SortKey::Potential => x
                        .analysis
                        .potential_score
                        .cmp(&y.analysis.potential_score)
                        .then(
                            x.analysis
                                .estimated_savings
                                .cmp(&y.analysis.estimated_savings),
                        ),
                    SortKey::Compatibility => {
                        x.analysis.compatibility.cmp(&y.analysis.compatibility)
                    }
                    SortKey::Name | SortKey::Folder | SortKey::Codec | SortKey::Health => {
                        std::cmp::Ordering::Equal
                    }
                }
            }),
        }
        if self.descending {
            rows.reverse();
        }
        self.cache = Some((ptr, key, rows.clone()));
        rows
    }
}

fn pixels(e: &LibraryEntry) -> u64 {
    e.media
        .primary_video()
        .map_or(0, |v| u64::from(v.width) * u64::from(v.height))
}

fn codec(e: &LibraryEntry) -> String {
    e.media
        .primary_video()
        .map(|v| v.codec.label().to_string())
        .unwrap_or_default()
}

fn rec_color(r: Recommendation) -> egui::Color32 {
    match r {
        Recommendation::AlreadyOptimal => theme::OK,
        Recommendation::Remux => theme::ACCENT,
        Recommendation::Transcode => theme::WARN,
        Recommendation::Review => theme::ERROR,
    }
}

fn health_color(s: HealthState) -> egui::Color32 {
    match s {
        HealthState::Ok => theme::OK,
        HealthState::Unchecked | HealthState::CheckFailed => theme::MUTED,
        HealthState::Risk => theme::ACCENT,
        HealthState::Suspect => theme::WARN,
        HealthState::Defect => theme::ERROR,
    }
}

fn severity_icon(s: Severity) -> (&'static str, egui::Color32) {
    match s {
        Severity::Risk => ("ℹ", theme::ACCENT),
        Severity::Suspect => ("⚠", theme::WARN),
        Severity::Defect => ("⛔", theme::ERROR),
    }
}

fn compat_color(c: Compatibility) -> egui::Color32 {
    match c {
        Compatibility::Compatible => theme::OK,
        Compatibility::Partial => theme::WARN,
        Compatibility::Incompatible => theme::ERROR,
    }
}

fn sort_header(ui: &mut egui::Ui, state: &mut LibraryUi, key: SortKey, label: &str) {
    let arrow = if state.sort == key {
        if state.descending { " ↓" } else { " ↑" }
    } else {
        ""
    };
    if ui
        .add(egui::Button::new(RichText::new(format!("{label}{arrow}")).strong()).frame(false))
        .clicked()
    {
        if state.sort == key {
            state.descending = !state.descending;
        } else {
            state.sort = key;
            state.descending = matches!(
                key,
                SortKey::Size
                    | SortKey::Bitrate
                    | SortKey::Health
                    | SortKey::Potential
                    | SortKey::Resolution
                    | SortKey::Duration
            );
        }
    }
}

pub fn show(app: &mut FramewiseApp, ui: &mut egui::Ui) {
    let lib = app.snap.library.clone();

    ui.horizontal(|ui| {
        ui.heading(tr!("Media library"));
        ui.add_space(12.0);
        scan_controls(app, ui);
    });

    // ---- filters
    let state = &mut app.library_ui;
    ui.horizontal_wrapped(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut state.search)
                .hint_text(tr!("Search file or folder…"))
                .desired_width(220.0),
        );
        let label = match state.rec_filter {
            RecFilter::All => tr!("All recommendations").to_string(),
            RecFilter::Actionable => tr!("Optimizable").to_string(),
            RecFilter::Only(r) => r.label().to_string(),
        };
        egui::ComboBox::from_id_salt("rec_filter")
            .selected_text(label)
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut state.rec_filter,
                    RecFilter::All,
                    tr!("All recommendations"),
                );
                ui.selectable_value(
                    &mut state.rec_filter,
                    RecFilter::Actionable,
                    tr!("Optimizable"),
                );
                for r in [
                    Recommendation::Transcode,
                    Recommendation::Remux,
                    Recommendation::AlreadyOptimal,
                    Recommendation::Review,
                ] {
                    ui.selectable_value(&mut state.rec_filter, RecFilter::Only(r), r.label());
                }
            });
        let label = match state.health_filter {
            HealthFilter::All => tr!("Any health").to_string(),
            HealthFilter::Problems => tr!("Problems").to_string(),
            HealthFilter::Only(s) => s.label().to_string(),
        };
        egui::ComboBox::from_id_salt("health_filter")
            .selected_text(label)
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut state.health_filter,
                    HealthFilter::All,
                    tr!("Any health"),
                );
                ui.selectable_value(
                    &mut state.health_filter,
                    HealthFilter::Problems,
                    tr!("Problems"),
                )
                .on_hover_text(tr!("Defect, suspect or playback risk"));
                for s in [
                    HealthState::Defect,
                    HealthState::Suspect,
                    HealthState::Risk,
                    HealthState::CheckFailed,
                    HealthState::Unchecked,
                    HealthState::Ok,
                ] {
                    ui.selectable_value(&mut state.health_filter, HealthFilter::Only(s), s.label());
                }
            });
        ui.checkbox(&mut state.only_hdr, "HDR");
        ui.checkbox(&mut state.only_top, tr!("Largest per folder"));
        ui.checkbox(&mut state.only_duplicates, tr!("Duplicates"));
        ui.checkbox(&mut state.hide_queued, tr!("Hide queued"));
    });

    let rows = app.library_ui.visible(&lib);
    // Media server items: report only, no queue.
    let remote = lib.first().is_some_and(|e| !e.media.is_local());
    poll_export(app);

    if remote {
        series_overview(app, ui, &lib);
    }

    health_controls(app, ui, &lib, &rows);

    // ---- selection / actions
    ui.horizontal_wrapped(|ui| {
        let n_sel = app.library_ui.selected.len();
        ui.label(trf!(
            "{} of {} shown · {} selected",
            rows.len(),
            lib.len(),
            n_sel
        ));
        if ui.button(tr!("Select shown")).clicked() {
            for &i in &rows {
                app.library_ui
                    .selected
                    .insert(lib[i].media.absolute_path.clone());
            }
        }
        if ui.button(tr!("Select none")).clicked() {
            app.library_ui.selected.clear();
        }
        ui.separator();
        if ui
            .add_enabled(
                !rows.is_empty() && app.library_ui.export.is_none(),
                egui::Button::new(trf!("Export shown as CSV ({})", rows.len())),
            )
            .clicked()
        {
            start_export(app, &lib, &rows);
        }
        if remote {
            return;
        }
        ui.separator();
        let profile_id = app.snap.config.encoding.active_profile.clone();
        let strategy = app.snap.config.encoding.conflict_strategy;
        if ui
            .add_enabled(
                n_sel > 0,
                egui::Button::new(trf!("Add selected to queue ({})", n_sel)),
            )
            .clicked()
        {
            app.send(Command::PreviewJobs {
                paths: selected_set(&app.library_ui.selected),
                profile_id: profile_id.clone(),
                strategy,
            });
        }
        let recommended = |i: &&usize| {
            let e = &lib[**i];
            !e.queued
                && matches!(
                    e.analysis.recommendation,
                    Recommendation::Transcode | Recommendation::Remux
                )
        };
        // Count every frame, collect the paths only on click.
        let n_actionable = rows.iter().filter(recommended).count();
        if ui
            .add_enabled(
                n_actionable > 0,
                egui::Button::new(trf!("Add all recommended shown ({})", n_actionable)),
            )
            .on_hover_text(tr!(
                "Only files with recommendation 'Re-encode' or 'Remux' that are not queued yet"
            ))
            .clicked()
        {
            app.send(Command::PreviewJobs {
                paths: rows
                    .iter()
                    .filter(recommended)
                    .map(|&i| lib[i].media.absolute_path.clone())
                    .collect(),
                profile_id,
                strategy,
            });
        }
    });
    ui.separator();

    // ---- details panel
    let index = app.library_ui.index(&lib);
    let focus_entry = app
        .library_ui
        .focus
        .as_ref()
        .and_then(|f| index.get(f))
        .map(|&i| &lib[i]);
    if let Some(entry) = focus_entry {
        egui::Panel::right("library_details")
            .resizable(true)
            .default_size(420.0)
            .min_size(300.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| details(ui, entry));
            });
    }

    // ---- table
    let state = &mut app.library_ui;
    let text_height = egui::TextStyle::Body.resolve(ui.style()).size + 6.0;
    let mut clicked_row: Option<PathBuf> = None;
    // horizontal scrolling keeps all columns reachable on narrow windows or
    // while the details panel is open
    egui::ScrollArea::horizontal().show(ui, |ui| {
        // selectable labels would swallow the row click
        ui.style_mut().interaction.selectable_labels = false;
        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .sense(egui::Sense::click())
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(22.0))
            .column(Column::initial(220.0).at_least(120.0).clip(true))
            .column(Column::initial(130.0).clip(true))
            .column(Column::initial(72.0))
            .column(Column::initial(84.0))
            .column(Column::initial(70.0))
            .column(Column::initial(84.0).clip(true))
            .column(Column::initial(70.0))
            .column(Column::initial(80.0).clip(true))
            .column(Column::initial(36.0))
            .column(Column::initial(60.0).clip(true))
            .column(Column::initial(90.0))
            .column(Column::initial(110.0).clip(true))
            .column(Column::remainder().at_least(130.0).clip(true))
            .header(22.0, |mut header| {
                header.col(|_| {});
                header.col(|ui| sort_header(ui, state, SortKey::Name, tr!("File name")));
                header.col(|ui| sort_header(ui, state, SortKey::Folder, tr!("Folder")));
                header.col(|ui| sort_header(ui, state, SortKey::Size, tr!("Size")));
                header.col(|ui| sort_header(ui, state, SortKey::Resolution, tr!("Resolution")));
                header.col(|ui| sort_header(ui, state, SortKey::Codec, "Codec"));
                header.col(|ui| sort_header(ui, state, SortKey::Bitrate, "Bitrate"));
                header.col(|ui| {
                    ui.strong("HDR");
                });
                header.col(|ui| {
                    ui.strong(tr!("Audio"));
                });
                header.col(|ui| {
                    ui.strong("#A");
                });
                header.col(|ui| {
                    ui.strong(tr!("Subs"));
                });
                header.col(|ui| sort_header(ui, state, SortKey::Compatibility, tr!("Compat.")));
                header.col(|ui| sort_header(ui, state, SortKey::Health, tr!("Health")));
                header.col(|ui| {
                    sort_header(
                        ui,
                        state,
                        SortKey::Potential,
                        tr!("Recommendation / potential"),
                    )
                });
            })
            .body(|body| {
                body.rows(text_height, rows.len(), |mut row| {
                    let e = &lib[rows[row.index()]];
                    let path = &e.media.absolute_path;
                    let is_selected = state.selected.contains(path);
                    row.set_selected(state.focus.as_ref() == Some(path));
                    row.col(|ui| {
                        let mut checked = is_selected;
                        if ui.checkbox(&mut checked, "").changed() {
                            if checked {
                                state.selected.insert(path.clone());
                            } else {
                                state.selected.remove(path);
                            }
                        }
                    });
                    row.col(|ui| {
                        let mut text = RichText::new(e.media.display_name());
                        if e.top_global {
                            text = text.strong();
                        }
                        ui.label(text)
                            .on_hover_text(e.media.relative_path.display().to_string());
                        if e.duplicate {
                            ui.label(RichText::new("⧉").color(theme::WARN))
                                .on_hover_text(tr!("Possible duplicate"));
                        }
                        if e.queued {
                            ui.label(RichText::new("⏳").color(theme::ACCENT))
                                .on_hover_text(tr!("In queue"));
                        }
                    });
                    row.col(|ui| {
                        ui.label(
                            RichText::new(e.media.folder().display().to_string())
                                .color(theme::MUTED),
                        );
                    });
                    row.col(|ui| {
                        let mut t = RichText::new(format::bytes(e.media.size_bytes)).monospace();
                        if e.top_in_folder {
                            t = t.color(theme::WARN);
                        }
                        ui.label(t).on_hover_text(if e.top_in_folder {
                            tr!("Among the largest files in this folder")
                        } else {
                            ""
                        });
                    });
                    let v = e.media.primary_video();
                    row.col(|ui| {
                        ui.label(v.map_or("–".into(), |v| format!("{}×{}", v.width, v.height)));
                    });
                    row.col(|ui| {
                        ui.label(v.map_or("–".into(), |v| v.codec.label().to_string()));
                    });
                    row.col(|ui| {
                        ui.label(format::bitrate(e.media.overall_bitrate()));
                    });
                    row.col(|ui| match v.map(|v| &v.hdr) {
                        Some(h) if h.is_hdr() => {
                            ui.label(RichText::new(h.label()).color(theme::HDR).small());
                        }
                        _ => {
                            ui.label(RichText::new("SDR").color(theme::MUTED).small());
                        }
                    });
                    row.col(|ui| {
                        ui.label(e.media.audio_languages().join(", "));
                    });
                    row.col(|ui| {
                        ui.label(e.media.audio.len().to_string());
                    });
                    row.col(|ui| {
                        let bitmap = e
                            .media
                            .subtitles
                            .iter()
                            .filter(|s| s.kind() == SubtitleKind::Bitmap)
                            .count();
                        let n = e.media.subtitles.len();
                        if bitmap > 0 {
                            ui.label(RichText::new(format!("{n} ({bitmap}B)")).color(theme::WARN))
                                .on_hover_text(tr!(
                                    "B = bitmap subtitles (PGS/VobSub), not MP4 compatible"
                                ));
                        } else {
                            ui.label(n.to_string());
                        }
                    });
                    row.col(|ui| {
                        let c = e.analysis.compatibility;
                        ui.label(RichText::new(c.label()).color(compat_color(c)).small());
                    });
                    row.col(|ui| {
                        let s = e.health.state();
                        let findings = e.health.findings();
                        let mut text = s.label().to_string();
                        if findings.len() > 1 {
                            text.push_str(&format!(" ({})", findings.len()));
                        }
                        let r = ui.label(RichText::new(text).color(health_color(s)).small());
                        if !findings.is_empty() {
                            r.on_hover_text(
                                findings
                                    .iter()
                                    .map(|f| {
                                        format!(
                                            "{} {}",
                                            severity_icon(f.severity()).0,
                                            f.describe()
                                        )
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                            );
                        } else if s == HealthState::Unchecked {
                            r.on_hover_text(tr!(
                                "No playback risk known – quick check not run yet"
                            ));
                        }
                    });
                    row.col(|ui| {
                        let r = e.analysis.recommendation;
                        ui.label(RichText::new(r.label()).color(rec_color(r)));
                        if let Some(s) = e.analysis.estimated_savings.filter(|s| *s > 0) {
                            ui.label(
                                RichText::new(format!("≈ −{}", format::bytes(s)))
                                    .small()
                                    .color(theme::MUTED),
                            )
                            .on_hover_text(tr!("Estimate – actual savings depend on the content"));
                        }
                        if !e.analysis.plan.warnings.is_empty() {
                            ui.label(
                                RichText::new(format!("⚠{}", e.analysis.plan.warnings.len()))
                                    .color(theme::WARN)
                                    .small(),
                            );
                        }
                    });
                    if row.response().clicked() {
                        clicked_row = Some(path.clone());
                    }
                });
            });
    });
    if let Some(p) = clicked_row {
        if app.library_ui.focus.as_ref() == Some(&p) {
            app.library_ui.focus = None;
        } else {
            app.library_ui.focus = Some(p);
        }
    }
}

fn kv(ui: &mut egui::Ui, k: &str, v: impl Into<String>) {
    ui.label(RichText::new(k).color(theme::MUTED));
    ui.label(v.into());
    ui.end_row();
}

fn opt<T: ToString>(v: &Option<T>) -> String {
    v.as_ref().map_or_else(|| "–".into(), ToString::to_string)
}

fn details(ui: &mut egui::Ui, e: &LibraryEntry) {
    let m = &e.media;
    let a = &e.analysis;
    ui.heading(m.display_name());
    match &m.provider {
        Some(p) => {
            ui.label(RichText::new(&p.server_path).small().color(theme::MUTED));
            ui.label(
                RichText::new(trf!(
                    "Stream information as reported by {} – the file itself was not read",
                    p.provider.label()
                ))
                .small()
                .color(theme::WARN),
            );
        }
        None => {
            ui.label(
                RichText::new(m.absolute_path.display().to_string())
                    .small()
                    .color(theme::MUTED),
            );
        }
    }
    ui.add_space(4.0);

    ui.horizontal(|ui| {
        ui.label(
            RichText::new(a.recommendation.label())
                .strong()
                .color(rec_color(a.recommendation)),
        );
        ui.label(RichText::new(a.compatibility.label()).color(compat_color(a.compatibility)));
        ui.label(trf!("Potential {}/100", a.potential_score));
    });
    if let Some(est) = a.estimated_size {
        ui.label(trf!(
            "Estimated result: ≈ {} (savings ≈ {}) – estimate, not guaranteed",
            format::bytes(est),
            format::opt_bytes(a.estimated_savings)
        ));
        let basis = match a.calibrated_from {
            Some(n) => trf!("Calibrated from {} finished jobs", n),
            None => tr!("Rule-of-thumb estimate – becomes more accurate after a few finished jobs")
                .to_string(),
        };
        ui.label(RichText::new(basis).small().color(theme::MUTED));
    }
    if let Some(bpp) = a.bits_per_pixel {
        ui.label(
            RichText::new(format!("{bpp:.3} bit/px"))
                .small()
                .color(theme::MUTED),
        );
    }

    health_details(ui, e);

    if !a.plan.blockers.is_empty() {
        ui.add_space(4.0);
        for b in &a.plan.blockers {
            ui.label(RichText::new(format!("⛔ {}", b.describe())).color(theme::ERROR));
        }
    }
    if let Some(vp) = &a.plan.video {
        for r in &vp.reasons {
            ui.label(RichText::new(format!("• {}", r.describe())).small());
        }
    }
    if !a.plan.warnings.is_empty() {
        ui.add_space(4.0);
        egui::CollapsingHeader::new(
            RichText::new(trf!("{} notes before encoding", a.plan.warnings.len()))
                .color(theme::WARN),
        )
        .default_open(true)
        .show(ui, |ui| {
            for w in &a.plan.warnings {
                ui.label(RichText::new(format!("⚠ {}", w.describe())).small());
            }
        });
    }

    ui.separator();
    egui::CollapsingHeader::new(tr!("General"))
        .default_open(true)
        .show(ui, |ui| {
            egui::Grid::new("general")
                .num_columns(2)
                .striped(true)
                .show(ui, |ui| {
                    kv(
                        ui,
                        tr!("Relative path"),
                        m.relative_path.display().to_string(),
                    );
                    kv(ui, tr!("Size"), format::bytes(m.size_bytes));
                    kv(ui, tr!("Modified"), format::timestamp(m.modified_unix));
                    kv(ui, tr!("Container"), m.container.format_name.clone());
                    kv(ui, tr!("Duration"), format::duration(m.duration_s()));
                    kv(ui, "Bitrate", format::bitrate(m.overall_bitrate()));
                    kv(ui, tr!("Streams"), m.container.stream_count.to_string());
                    kv(ui, tr!("Chapters"), m.container.chapters.to_string());
                });
        });

    for v in &m.video {
        egui::CollapsingHeader::new(trf!("Video #{} – {}", v.index, v.codec.label()))
            .default_open(!v.attached_pic)
            .show(ui, |ui| {
                egui::Grid::new(("video", v.index))
                    .num_columns(2)
                    .striped(true)
                    .show(ui, |ui| {
                        kv(ui, tr!("Profile"), opt(&v.profile));
                        kv(ui, tr!("Resolution"), format!("{}×{}", v.width, v.height));
                        kv(
                            ui,
                            tr!("Aspect ratio"),
                            format!(
                                "DAR {} · SAR {}",
                                opt(&v.display_aspect_ratio),
                                opt(&v.sample_aspect_ratio)
                            ),
                        );
                        kv(
                            ui,
                            "Framerate",
                            v.fps().map_or("–".into(), |f| format!("{f:.3} fps")),
                        );
                        kv(ui, "Bitrate", format::bitrate(v.bit_rate));
                        kv(ui, tr!("Pixel format"), opt(&v.pix_fmt));
                        kv(ui, tr!("Bit depth"), opt(&v.bit_depth));
                        kv(
                            ui,
                            tr!("Color space"),
                            format!(
                                "{} / {} / {} / {}",
                                opt(&v.color.primaries),
                                opt(&v.color.transfer),
                                opt(&v.color.space),
                                opt(&v.color.range)
                            ),
                        );
                        kv(ui, "HDR", v.hdr.label());
                        if let HdrFormat::DolbyVision(dv) = &v.hdr {
                            kv(
                                ui,
                                "Dolby Vision",
                                format!(
                                    "P{} L{} EL={}",
                                    opt(&dv.profile),
                                    opt(&dv.level),
                                    dv.has_enhancement_layer
                                ),
                            );
                        }
                        if let Some(r) = v.rotation {
                            kv(ui, tr!("Rotation"), format!("{r}°"));
                        }
                        if let Some(i) = v.interlaced {
                            kv(
                                ui,
                                tr!("Interlaced"),
                                if i { tr!("yes") } else { tr!("no") },
                            );
                        }
                        if v.attached_pic {
                            kv(ui, tr!("Type"), tr!("Cover art"));
                        }
                    });
            });
    }

    egui::CollapsingHeader::new(trf!("Audio ({})", m.audio.len()))
        .default_open(true)
        .show(ui, |ui| {
            for t in &m.audio {
                let flags = [
                    t.default.then_some(tr!("default")),
                    t.forced.then_some(tr!("forced")),
                    t.commentary.then_some(tr!("commentary")),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(", ");
                ui.label(
                    RichText::new(format!(
                        "#{} {} · {} · {} · {} Hz · {}",
                        t.index,
                        t.language.as_deref().unwrap_or("und"),
                        t.codec.label(),
                        t.channel_label(),
                        opt(&t.sample_rate),
                        format::bitrate(t.bit_rate)
                    ))
                    .strong(),
                );
                ui.label(
                    RichText::new(format!(
                        "{} {} {}",
                        t.title.as_deref().unwrap_or(""),
                        t.profile
                            .as_deref()
                            .map(|p| format!("[{p}]"))
                            .unwrap_or_default(),
                        if flags.is_empty() {
                            String::new()
                        } else {
                            format!("({flags})")
                        }
                    ))
                    .small()
                    .color(theme::MUTED),
                );
                if t.atmos.is_present() {
                    ui.label(
                        RichText::new(format!("Dolby Atmos: {}", t.atmos.describe()))
                            .small()
                            .color(theme::HDR),
                    );
                }
                if t.dts_x.is_present() {
                    ui.label(
                        RichText::new(format!("DTS:X: {}", t.dts_x.describe()))
                            .small()
                            .color(theme::HDR),
                    );
                }
            }
        });

    egui::CollapsingHeader::new(trf!("Subtitles ({})", m.subtitles.len()))
        .default_open(true)
        .show(ui, |ui| {
            for s in &m.subtitles {
                let kind = match s.kind() {
                    SubtitleKind::Text => tr!("text"),
                    SubtitleKind::Bitmap => tr!("bitmap"),
                    SubtitleKind::Unknown => "?",
                };
                let flags = [
                    s.default.then_some(tr!("default")),
                    s.forced.then_some(tr!("forced")),
                    s.hearing_impaired.then_some("SDH"),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(", ");
                let color = if s.kind() == SubtitleKind::Text {
                    egui::Color32::WHITE
                } else {
                    theme::WARN
                };
                ui.label(
                    RichText::new(format!(
                        "#{} {} · {} ({kind}) {} {}",
                        s.index,
                        s.language.as_deref().unwrap_or("und"),
                        s.codec.label(),
                        s.title.as_deref().unwrap_or(""),
                        if flags.is_empty() {
                            String::new()
                        } else {
                            format!("({flags})")
                        }
                    ))
                    .color(color),
                );
            }
        });

    if !m.other_streams.is_empty() {
        egui::CollapsingHeader::new(tr!("Other streams")).show(ui, |ui| {
            for s in &m.other_streams {
                ui.label(RichText::new(s).small());
            }
        });
    }

    egui::CollapsingHeader::new(tr!("Planned output"))
        .default_open(true)
        .show(ui, |ui| {
            if let Some(vp) = &a.plan.video {
                let text = match &vp.action {
                    VideoAction::Copy => tr!("Video: copy (unchanged)").to_string(),
                    VideoAction::Encode {
                        codec,
                        crf,
                        resolution,
                        hdr,
                        deinterlace,
                        ten_bit,
                    } => trf!(
                        "Video: {} CRF {} → {}{}{}",
                        codec.label(),
                        crf,
                        resolution.output,
                        {
                            use crate::encoding::plan::{HdrConversion, HdrTransfer};
                            let range = match hdr {
                                HdrConversion::TonemapToSdr { .. } => " · HDR→SDR".to_string(),
                                HdrConversion::Preserve { source } => HdrTransfer::of(source)
                                    .map(|t| format!(" · {}", t.label()))
                                    .unwrap_or_default(),
                                HdrConversion::None => String::new(),
                            };
                            let depth = if *ten_bit { " · 10 bit" } else { "" };
                            format!("{range}{depth}")
                        },
                        if *deinterlace { " · deinterlace" } else { "" }
                    ),
                };
                ui.label(text);
            }
            for (k, o) in a.plan.audio.iter().enumerate() {
                let action = match &o.action {
                    AudioAction::Copy => tr!("copy").to_string(),
                    AudioAction::Transcode {
                        codec,
                        bitrate_k,
                        channels,
                    } => {
                        format!("{} {channels}ch {bitrate_k}k", codec.label())
                    }
                };
                ui.label(format!(
                    "A{k}: {} ← #{} · {}{}",
                    o.language.as_deref().unwrap_or("und"),
                    o.source_type_index,
                    action,
                    if o.default { " · default" } else { "" }
                ));
            }
            for s in &a.plan.subtitles {
                let action = match &s.action {
                    SubtitleAction::MovText => "mov_text".to_string(),
                    SubtitleAction::Sidecar { format, suffix } => {
                        format!("sidecar .{suffix}.{}", format.extension())
                    }
                    SubtitleAction::Skip => tr!("not kept").to_string(),
                };
                ui.label(format!(
                    "S: {} ← #{} · {}",
                    s.language.as_deref().unwrap_or("und"),
                    s.source_type_index,
                    action
                ));
            }
        });
}

/// Save dialog on its own thread; the CSV is built right away from the
/// rows shown now and written once a path was chosen.
fn start_export(app: &mut FramewiseApp, lib: &Arc<Vec<LibraryEntry>>, rows: &[usize]) {
    let csv = report::to_csv(
        rows.iter()
            .map(|&i| (&lib[i].media, &lib[i].analysis, &lib[i].health)),
    );
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let path = rfd::FileDialog::new()
            .set_file_name("framewise.csv")
            .add_filter("CSV", &["csv"])
            .save_file();
        let _ = tx.send(path);
    });
    app.library_ui.export = Some((csv, rx));
}

fn poll_export(app: &mut FramewiseApp) {
    let Some((_, rx)) = &app.library_ui.export else {
        return;
    };
    let path = match rx.try_recv() {
        Ok(path) => path,
        Err(mpsc::TryRecvError::Empty) => return,
        Err(mpsc::TryRecvError::Disconnected) => None,
    };
    let Some((csv, _)) = app.library_ui.export.take() else {
        return;
    };
    if let Some(path) = path {
        match std::fs::write(&path, csv) {
            Ok(()) => app.notify(NoticeLevel::Info, trf!("Exported to {}", path.display())),
            Err(e) => app.notify(
                NoticeLevel::Error,
                trf!("Export to {} failed: {}", path.display(), e),
            ),
        }
    }
}

/// `12 of 40 to process (8 re-encode, 4 remux) · 2 review`
fn counts_text(c: &Counts) -> String {
    let mut text = trf!(
        "{} of {} to process ({} re-encode, {} remux)",
        c.actionable(),
        c.total,
        c.transcode,
        c.remux
    );
    if c.review > 0 {
        text.push_str(&trf!(" · {} to review", c.review));
    }
    if c.defect > 0 {
        text.push_str(&trf!(" · {} defect", c.defect));
    }
    if c.suspect > 0 {
        text.push_str(&trf!(" · {} suspect", c.suspect));
    }
    text
}

fn counts_color(c: &Counts) -> egui::Color32 {
    if c.review > 0 || c.defect > 0 {
        theme::ERROR
    } else if c.actionable() > 0 {
        theme::WARN
    } else {
        theme::OK
    }
}

/// Movies and series → seasons with the number of items to process.
/// Clicking a series filters the table to it.
fn series_overview(app: &mut FramewiseApp, ui: &mut egui::Ui, lib: &Arc<Vec<LibraryEntry>>) {
    let ptr = Arc::as_ptr(lib) as usize;
    let summary = match &app.library_ui.overview {
        Some((p, s)) if *p == ptr => s.clone(),
        _ => {
            let s = Arc::new(summary::summarize(lib.iter().filter_map(|e| {
                e.media
                    .provider
                    .as_ref()
                    .map(|p| (p, e.analysis.recommendation, e.health.state()))
            })));
            app.library_ui.overview = Some((ptr, s.clone()));
            s
        }
    };
    egui::CollapsingHeader::new(RichText::new(tr!("Overview by series")).strong())
        .id_salt("series_overview")
        .default_open(false)
        .show(ui, |ui| {
            if summary.movies.total > 0 {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(tr!("Movies")).strong());
                    ui.label(
                        RichText::new(counts_text(&summary.movies))
                            .color(counts_color(&summary.movies)),
                    );
                });
            }
            if summary.episodes.total > 0 {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(tr!("Episodes")).strong());
                    ui.label(
                        RichText::new(counts_text(&summary.episodes))
                            .color(counts_color(&summary.episodes)),
                    );
                });
            }
            egui::ScrollArea::vertical()
                .id_salt("series_overview_scroll")
                .max_height(260.0)
                .show(ui, |ui| {
                    for series in &summary.series {
                        let name = if series.name.is_empty() {
                            tr!("Unknown series").to_string()
                        } else {
                            series.name.clone()
                        };
                        let header = egui::CollapsingHeader::new(
                            RichText::new(format!("{name}  –  {}", counts_text(&series.counts)))
                                .color(counts_color(&series.counts)),
                        )
                        .id_salt(("series", &series.name))
                        .show(ui, |ui| {
                            for season in &series.seasons {
                                let label = match season.season {
                                    Some(0) => tr!("Specials").to_string(),
                                    Some(n) => trf!("Season {}", n),
                                    None => tr!("Without season").to_string(),
                                };
                                ui.label(
                                    RichText::new(format!(
                                        "{label}: {}",
                                        counts_text(&season.counts)
                                    ))
                                    .color(counts_color(&season.counts)),
                                );
                            }
                            if ui.small_button(tr!("Show in table")).clicked() {
                                app.library_ui.search = series.name.clone();
                            }
                        });
                        header
                            .header_response
                            .on_hover_text(tr!("Expand for the seasons"));
                    }
                });
        });
    ui.separator();
}

/// Quick check buttons and progress.
fn health_controls(
    app: &mut FramewiseApp,
    ui: &mut egui::Ui,
    lib: &Arc<Vec<LibraryEntry>>,
    rows: &[usize],
) {
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(tr!("Quick check")).strong())
            .on_hover_text(tr!("Reads the header and a few short samples of each file (through the server for Jellyfin) and looks for broken data, audio gaps and audio/video offsets."));
        if let Some(p) = app.snap.health_progress.clone() {
            ui.add(
                egui::ProgressBar::new(p.done as f32 / p.total.max(1) as f32)
                    .desired_width(220.0)
                    .text(format!("{} / {}", p.done, p.total)),
            );
            ui.label(
                RichText::new(trf!("{} defect · {} suspect", p.defect, p.suspect)).color(
                    if p.defect > 0 { theme::ERROR } else { theme::MUTED },
                ),
            );
            if ui.button(tr!("Stop")).on_hover_text(tr!("Results so far are kept; starting again continues with the unchecked files")).clicked() {
                app.send(Command::CancelHealthCheck);
            }
            ui.label(RichText::new(p.current).small().color(theme::MUTED));
            return;
        }
        // Runs every frame on libraries with tens of thousands of rows:
        // count without copying paths; build the lists only on click.
        let recheck = app.library_ui.recheck;
        let pending = |i: usize| recheck || !lib[i].health.checked();
        let n_shown = rows.iter().filter(|&&i| pending(i)).count();
        if ui
            .add_enabled(n_shown > 0, egui::Button::new(trf!("Check shown ({})", n_shown)))
            .clicked()
        {
            app.send(Command::CheckHealth {
                paths: rows
                    .iter()
                    .map(|&i| lib[i].media.absolute_path.clone())
                    .collect(),
                recheck,
            });
        }
        let index = app.library_ui.index(lib);
        let n_sel = app
            .library_ui
            .selected
            .iter()
            .filter_map(|p| index.get(p))
            .filter(|&&i| pending(i))
            .count();
        if ui
            .add_enabled(n_sel > 0, egui::Button::new(trf!("Check selected ({})", n_sel)))
            .clicked()
        {
            app.send(Command::CheckHealth {
                paths: selected_set(&app.library_ui.selected),
                recheck,
            });
        }
        ui.checkbox(&mut app.library_ui.recheck, tr!("check again"))
            .on_hover_text(tr!("Also check files that were checked already"));
    });
}

fn health_details(ui: &mut egui::Ui, e: &LibraryEntry) {
    let h = &e.health;
    let state = h.state();
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new(tr!("Health")).strong());
        ui.label(RichText::new(state.label()).color(health_color(state)));
        match &h.check {
            Some(c) if c.failed.is_none() => {
                ui.label(
                    RichText::new(trf!(
                        "checked {} · {} samples",
                        format::timestamp(c.checked_unix),
                        c.samples
                    ))
                    .small()
                    .color(theme::MUTED),
                );
            }
            Some(c) => {
                ui.label(
                    RichText::new(trf!(
                        "check failed: {}",
                        c.failed.as_deref().unwrap_or_default()
                    ))
                    .small()
                    .color(theme::WARN),
                );
            }
            None => {
                ui.label(
                    RichText::new(tr!("quick check not run yet"))
                        .small()
                        .color(theme::MUTED),
                );
            }
        }
    });
    for f in h.findings() {
        let (icon, color) = severity_icon(f.severity());
        ui.label(
            RichText::new(format!("{icon} {}", f.describe()))
                .color(color)
                .small(),
        );
    }
    if !h.risks.is_empty() {
        ui.label(
            RichText::new(tr!("ℹ = deviates from the most compatible format: not every TV plays it directly, the server converts on the fly, which can stall on weak servers or Wi-Fi."))
                .small()
                .color(theme::MUTED),
        );
    }
}
