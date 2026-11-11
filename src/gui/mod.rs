//! egui front end.
//!
//! The GUI only holds snapshots received from the backend plus pure view
//! state (filters, sorting, selection, form drafts). It never blocks: all
//! work is requested via [`Command`]s.

pub(crate) mod format;
mod theme;
mod views;

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;

use crate::core::backend::BackendHandle;
use crate::core::config::AppConfig;
use crate::core::events::{AppEvent, Command, JobPreviewItem, Library, NoticeLevel};
use crate::core::tools::ToolInfo;
use crate::encoding::hardware::HardwareCaps;
use crate::encoding::profile::EncodingProfile;
use crate::monitor::{GateBlock, ResourceSnapshot};
use crate::queue::job::{HistoryEntry, Job, JobState};
use crate::scanner::{DuplicateGroup, ScanIssue, ScanProgress};
use crate::{i18n, tr};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Dashboard,
    Library,
    Profiles,
    Queue,
    Hardware,
    History,
    Settings,
}

impl View {
    const ALL: [View; 7] = [
        View::Dashboard,
        View::Library,
        View::Profiles,
        View::Queue,
        View::Hardware,
        View::History,
        View::Settings,
    ];

    fn label(self) -> &'static str {
        match self {
            View::Dashboard => tr!("Dashboard"),
            View::Library => tr!("Media library"),
            View::Profiles => tr!("Encoding profiles"),
            View::Queue => tr!("Queue"),
            View::Hardware => tr!("Hardware monitor"),
            View::History => tr!("History"),
            View::Settings => tr!("Settings"),
        }
    }
}

pub struct Notice {
    pub level: NoticeLevel,
    pub text: String,
    pub at: Instant,
}

/// Summary of the last scan.
#[derive(Default)]
pub struct ScanSummary {
    pub files: usize,
    pub reused: usize,
    pub issues: Vec<ScanIssue>,
    pub duplicates: Vec<DuplicateGroup>,
    pub cancelled: bool,
}

pub struct PreviewState {
    pub profile_id: String,
    pub items: Vec<JobPreviewItem>,
    pub confirm_replace: bool,
}

/// Snapshots received from the backend.
#[derive(Default)]
pub struct Snapshots {
    pub config: AppConfig,
    pub tools: Option<ToolInfo>,
    pub caps: Option<Arc<HardwareCaps>>,
    pub library: Library,
    pub jobs: Vec<Job>,
    pub profiles: Vec<EncodingProfile>,
    pub history: Vec<HistoryEntry>,
    pub resources: ResourceSnapshot,
    pub cpu_history: VecDeque<f32>,
    pub queue_paused: bool,
    pub gate: Option<GateBlock>,
    pub scan_progress: Option<ScanProgress>,
    pub scan_summary: Option<ScanSummary>,
}

pub struct FramewiseApp {
    backend: BackendHandle,
    pub snap: Snapshots,
    pub view: View,
    pub notices: VecDeque<Notice>,
    pub preview: Option<PreviewState>,
    pub library_ui: views::library::LibraryUi,
    pub queue_ui: views::queue::QueueUi,
    pub profiles_ui: views::profiles::ProfilesUi,
    pub settings_ui: views::settings::SettingsUi,
    pub history_ui: views::history::HistoryUi,
    applied_scale: f32,
}

impl FramewiseApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        backend: BackendHandle,
        config: AppConfig,
    ) -> Self {
        theme::apply(&cc.egui_ctx);
        let scale = config.ui.scale;
        cc.egui_ctx.set_zoom_factor(scale);
        Self {
            backend,
            snap: Snapshots {
                config: config.clone(),
                ..Default::default()
            },
            view: View::Dashboard,
            notices: VecDeque::new(),
            preview: None,
            library_ui: Default::default(),
            queue_ui: Default::default(),
            profiles_ui: Default::default(),
            settings_ui: views::settings::SettingsUi::new(&config),
            history_ui: Default::default(),
            applied_scale: scale,
        }
    }

    pub fn send(&self, cmd: Command) {
        self.backend.send(cmd);
    }

    pub fn notify(&mut self, level: NoticeLevel, text: impl Into<String>) {
        self.notices.push_back(Notice {
            level,
            text: text.into(),
            at: Instant::now(),
        });
        while self.notices.len() > 50 {
            self.notices.pop_front();
        }
    }

    pub fn active_profile(&self) -> Option<&EncodingProfile> {
        self.snap
            .profiles
            .iter()
            .find(|p| p.id == self.snap.config.encoding.active_profile)
    }

    /// Change config immediately (persisted by the backend).
    pub fn update_config(&mut self, f: impl FnOnce(&mut AppConfig)) {
        let mut cfg = self.snap.config.clone();
        f(&mut cfg);
        self.snap.config = cfg.clone();
        self.send(Command::UpdateConfig(Box::new(cfg)));
    }

    pub fn scan_running(&self) -> bool {
        self.snap.scan_progress.is_some()
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.backend.events.try_recv() {
            match event {
                AppEvent::Tools(info) => self.snap.tools = Some(info),
                AppEvent::Hardware(caps) => self.snap.caps = Some(caps),
                AppEvent::Config(cfg) => {
                    i18n::set_language(cfg.ui.language.unwrap_or_else(i18n::Language::from_env));
                    self.snap.config = *cfg;
                }
                AppEvent::ScanProgress(p) => self.snap.scan_progress = Some(p),
                AppEvent::ScanFinished {
                    files,
                    reused,
                    issues,
                    duplicates,
                    cancelled,
                } => {
                    self.snap.scan_progress = None;
                    let text = if cancelled {
                        tr!("Scan cancelled").to_string()
                    } else {
                        crate::trf!(
                            "Scan finished: {} videos ({} unchanged from cache), {} problems",
                            files,
                            reused,
                            issues.len()
                        )
                    };
                    self.notify(
                        if issues.is_empty() {
                            NoticeLevel::Info
                        } else {
                            NoticeLevel::Warning
                        },
                        text,
                    );
                    self.snap.scan_summary = Some(ScanSummary {
                        files,
                        reused,
                        issues,
                        duplicates,
                        cancelled,
                    });
                }
                AppEvent::Library(lib) => {
                    self.snap.library = lib;
                    self.library_ui.invalidate();
                }
                AppEvent::Jobs(jobs) => self.snap.jobs = jobs,
                AppEvent::JobUpdated(job) => {
                    match self.snap.jobs.iter_mut().find(|j| j.id == job.id) {
                        Some(slot) => *slot = *job,
                        None => self.snap.jobs.push(*job),
                    }
                }
                AppEvent::QueueState { paused, gate } => {
                    self.snap.queue_paused = paused;
                    self.snap.gate = gate;
                }
                AppEvent::Resources(r) => {
                    self.snap.cpu_history.push_back(r.cpu_percent);
                    while self.snap.cpu_history.len() > 120 {
                        self.snap.cpu_history.pop_front();
                    }
                    self.snap.resources = *r;
                }
                AppEvent::Profiles(p) => {
                    self.snap.profiles = p;
                    self.profiles_ui.on_profiles_changed(&self.snap.profiles);
                }
                AppEvent::History(h) => self.snap.history = h,
                AppEvent::JobPreview { profile_id, items } => {
                    self.preview = Some(PreviewState {
                        profile_id,
                        items,
                        confirm_replace: false,
                    });
                }
                AppEvent::Notice { level, text } => self.notify(level, text),
            }
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading(
                egui::RichText::new("Framewise")
                    .color(theme::ACCENT)
                    .strong(),
            );
            ui.label(egui::RichText::new(tr!("Media Optimizer")).color(theme::MUTED));
            ui.separator();

            ui.label(tr!("Profile:"));
            let current = self
                .active_profile()
                .map(|p| p.name.clone())
                .unwrap_or_else(|| "–".into());
            let mut selected = self.snap.config.encoding.active_profile.clone();
            egui::ComboBox::from_id_salt("active_profile")
                .selected_text(current)
                .show_ui(ui, |ui| {
                    for p in &self.snap.profiles {
                        ui.selectable_value(
                            &mut selected,
                            p.id.clone(),
                            format!("{} – {}", p.name, p.kind.label()),
                        );
                    }
                });
            if selected != self.snap.config.encoding.active_profile {
                self.update_config(|c| c.encoding.active_profile = selected);
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let running = self
                    .snap
                    .jobs
                    .iter()
                    .filter(|j| j.state.is_active())
                    .count();
                let queued = self
                    .snap
                    .jobs
                    .iter()
                    .filter(|j| j.state == JobState::Queued)
                    .count();
                if running > 0 {
                    ui.label(
                        egui::RichText::new(crate::trf!(
                            "▶ {} active · {} waiting",
                            running,
                            queued
                        ))
                        .color(theme::OK),
                    );
                } else if queued > 0 {
                    ui.label(
                        egui::RichText::new(crate::trf!("{} waiting", queued)).color(theme::MUTED),
                    );
                }
                if self.snap.queue_paused && queued > 0 {
                    ui.label(egui::RichText::new(tr!("⏸ Queue paused")).color(theme::WARN));
                }
                if let Some(p) = &self.snap.scan_progress {
                    views::dashboard::scan_progress_label(ui, p);
                }
            });
        });
    }

    fn nav(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        for view in View::ALL {
            let mut text = egui::RichText::new(view.label());
            if view == View::Queue {
                let n = self
                    .snap
                    .jobs
                    .iter()
                    .filter(|j| !j.state.is_finished())
                    .count();
                if n > 0 {
                    text = egui::RichText::new(format!("{}  ({n})", view.label()));
                }
            }
            if ui
                .add_sized(
                    [ui.available_width(), 28.0],
                    egui::Button::selectable(self.view == view, text),
                )
                .clicked()
            {
                self.view = view;
            }
        }
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            ui.add_space(4.0);
            match &self.snap.tools {
                Some(t) if t.ready() => {
                    ui.label(egui::RichText::new("ffmpeg ✔").small().color(theme::OK));
                }
                Some(_) => {
                    ui.label(
                        egui::RichText::new(tr!("ffmpeg missing"))
                            .small()
                            .color(theme::ERROR),
                    );
                }
                None => {
                    ui.label(
                        egui::RichText::new(tr!("Checking ffmpeg…"))
                            .small()
                            .color(theme::MUTED),
                    );
                }
            }
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let r = &self.snap.resources;
            ui.label(egui::RichText::new(format!("CPU {:.0} %", r.cpu_percent)).small());
            if r.mem_total > 0 {
                let used = r.mem_total.saturating_sub(r.mem_available);
                ui.label(
                    egui::RichText::new(format!(
                        "RAM {} / {}",
                        format::bytes(used),
                        format::bytes(r.mem_total)
                    ))
                    .small(),
                );
            }
            for g in &r.gpus {
                if let Some(u) = g.utilization {
                    ui.label(egui::RichText::new(format!("GPU {u:.0} %")).small());
                }
            }
            if let Some(gate) = &self.snap.gate {
                ui.label(
                    egui::RichText::new(format!("⏳ {}", gate.describe()))
                        .small()
                        .color(theme::WARN),
                );
            }
            ui.separator();
            if let Some(n) = self.notices.back() {
                let color = match n.level {
                    NoticeLevel::Info => theme::MUTED,
                    NoticeLevel::Warning => theme::WARN,
                    NoticeLevel::Error => theme::ERROR,
                };
                if n.at.elapsed() < Duration::from_secs(30) || n.level == NoticeLevel::Error {
                    ui.label(egui::RichText::new(&n.text).small().color(color))
                        .on_hover_text(&n.text);
                }
            }
        });
    }
}

impl eframe::App for FramewiseApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_events();
        let ctx = ui.ctx().clone();
        if (self.snap.config.ui.scale - self.applied_scale).abs() > f32::EPSILON {
            self.applied_scale = self.snap.config.ui.scale;
            ctx.set_zoom_factor(self.applied_scale);
        }
        // Keep live values (progress, monitor) moving even without input.
        ctx.request_repaint_after(Duration::from_millis(500));

        egui::Panel::top("top_bar").show(ui, |ui| {
            ui.add_space(4.0);
            self.top_bar(ui);
            ui.add_space(2.0);
        });
        egui::Panel::bottom("status_bar").show(ui, |ui| self.status_bar(ui));
        egui::Panel::left("nav")
            .resizable(false)
            .exact_size(170.0)
            .show(ui, |ui| self.nav(ui));
        egui::CentralPanel::default().show(ui, |ui| match self.view {
            View::Dashboard => views::dashboard::show(self, ui),
            View::Library => views::library::show(self, ui),
            View::Profiles => views::profiles::show(self, ui),
            View::Queue => views::queue::show(self, ui),
            View::Hardware => views::hardware::show(self, ui),
            View::History => views::history::show(self, ui),
            View::Settings => views::settings::show(self, ui),
        });

        views::preview::show(self, &ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.backend.shutdown();
    }
}

/// Selected media paths helper used by several views.
pub fn paths_of(entries: &[usize], library: &Library) -> Vec<PathBuf> {
    entries
        .iter()
        .filter_map(|&i| library.get(i))
        .map(|e| e.media.absolute_path.clone())
        .collect()
}

pub fn selected_set(set: &HashSet<PathBuf>) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = set.iter().cloned().collect();
    v.sort();
    v
}
