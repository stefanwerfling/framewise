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
use crate::queue::job::{HistoryEntry, Job, JobId, JobState};
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

    fn icon(self) -> &'static str {
        match self {
            View::Dashboard => "⊞",
            View::Library => "▤",
            View::Profiles => "☰",
            View::Queue => "▶",
            View::Hardware => "▥",
            View::History => "⟲",
            View::Settings => "⚙",
        }
    }

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
    /// Job the notice is about (offers a retry while it can be retried).
    pub job: Option<JobId>,
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
    pub health_progress: Option<crate::core::events::HealthProgress>,
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
        self.push_notice(level, text.into(), None);
    }

    fn push_notice(&mut self, level: NoticeLevel, text: String, job: Option<JobId>) {
        self.notices.push_back(Notice {
            level,
            text,
            at: Instant::now(),
            job,
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
                    i18n::set_language(cfg.ui.language.unwrap_or_default());
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
                    let failed = cancelled && !issues.is_empty();
                    let text = if failed {
                        crate::trf!("Scan failed: {}", issues[0].message)
                    } else if cancelled {
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
                        if failed {
                            NoticeLevel::Error
                        } else if issues.is_empty() {
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
                AppEvent::Notice { level, text, job } => self.push_notice(level, text, job),
                AppEvent::Jellyfin(result) => self.settings_ui.on_jellyfin(result),
                AppEvent::QuickConnect(event) => self.settings_ui.on_quick_connect(event),
                AppEvent::HealthProgress(p) => self.snap.health_progress = p,
            }
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(tr!("Active profile")).color(theme::MUTED));
            let current = self
                .active_profile()
                .map(|p| p.name.clone())
                .unwrap_or_else(|| "–".into());
            let mut selected = self.snap.config.encoding.active_profile.clone();
            egui::ComboBox::from_id_salt("active_profile")
                .selected_text(egui::RichText::new(current).strong())
                .width(220.0)
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
                if self.snap.queue_paused && queued > 0 {
                    theme::badge(ui, tr!("‖ Queue paused"), theme::WARN);
                }
                if queued > 0 {
                    theme::badge(ui, crate::trf!("{} waiting", queued), theme::MUTED);
                }
                if running > 0 {
                    theme::badge(ui, crate::trf!("▶ {} active", running), theme::OK);
                }
                if let Some(p) = &self.snap.scan_progress {
                    views::dashboard::scan_progress_label(ui, p);
                }
            });
        });
    }

    fn nav(&mut self, ui: &mut egui::Ui) {
        // brand
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            let (rect, _) = ui.allocate_exact_size(egui::vec2(30.0, 30.0), egui::Sense::hover());
            let painter = ui.painter();
            painter.rect_filled(rect, 8.0, theme::ACCENT);
            // three stacked frames: a stylised film strip
            for (i, w) in [14.0_f32, 10.0, 6.0].iter().enumerate() {
                let y = rect.top() + 9.0 + i as f32 * 5.0;
                painter.rect_filled(
                    egui::Rect::from_min_size(
                        egui::pos2(rect.left() + 8.0, y),
                        egui::vec2(*w, 3.0),
                    ),
                    1.5,
                    theme::ON_ACCENT,
                );
            }
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(
                    egui::RichText::new("Framewise")
                        .size(16.0)
                        .strong()
                        .color(theme::TEXT_STRONG),
                );
                ui.label(
                    egui::RichText::new(tr!("Media Optimizer"))
                        .small()
                        .color(theme::MUTED),
                );
            });
        });
        ui.add_space(18.0);

        let open_jobs = self
            .snap
            .jobs
            .iter()
            .filter(|j| !j.state.is_finished())
            .count();
        for view in View::ALL {
            let badge = (view == View::Queue && open_jobs > 0).then_some(open_jobs);
            if nav_item(ui, view.icon(), view.label(), self.view == view, badge).clicked() {
                self.view = view;
            }
        }

        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                        .small()
                        .color(theme::MUTED),
                );
                match &self.snap.tools {
                    Some(t) if t.ready() => {
                        theme::badge(ui, "ffmpeg ✔", theme::OK);
                    }
                    Some(_) => {
                        theme::badge(ui, tr!("ffmpeg missing"), theme::ERROR);
                    }
                    None => {
                        theme::badge(ui, tr!("Checking ffmpeg…"), theme::MUTED);
                    }
                }
            });
            ui.add_space(6.0);
            self.language_switch(ui);
            ui.add_space(4.0);
            ui.separator();
        });
    }

    /// EN | DE segmented switch; applies at once and is saved.
    fn language_switch(&mut self, ui: &mut egui::Ui) {
        let current = i18n::language();
        let mut chosen = None;
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(tr!("Language"))
                    .small()
                    .color(theme::MUTED),
            );
            ui.add_space(4.0);
            ui.spacing_mut().item_spacing.x = 2.0;
            for lang in i18n::Language::ALL {
                let on = lang == current;
                let text = egui::RichText::new(lang.code())
                    .small()
                    .strong()
                    .color(if on { theme::ON_ACCENT } else { theme::TEXT });
                let button = egui::Button::new(text)
                    .fill(if on { theme::ACCENT } else { theme::SURFACE })
                    .stroke(egui::Stroke::new(1.0, theme::BORDER))
                    .min_size(egui::vec2(34.0, 20.0));
                if ui.add(button).on_hover_text(lang.native_name()).clicked() && !on {
                    chosen = Some(lang);
                }
            }
        });
        if let Some(lang) = chosen {
            self.set_language(lang);
        }
    }

    pub fn set_language(&mut self, lang: i18n::Language) {
        i18n::set_language(lang);
        self.settings_ui.on_language_changed(lang);
        self.update_config(|c| c.ui.language = Some(lang));
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let r = &self.snap.resources;
            let cpu = r.cpu_percent / 100.0;
            ui.label(egui::RichText::new("CPU").small().color(theme::MUTED));
            theme::meter(ui, 56.0, cpu, theme::load_color(cpu));
            ui.label(egui::RichText::new(format!("{:.0} %", r.cpu_percent)).small());
            if r.mem_total > 0 {
                let used = r.mem_total.saturating_sub(r.mem_available);
                let frac = used as f32 / r.mem_total as f32;
                ui.add_space(6.0);
                ui.label(egui::RichText::new("RAM").small().color(theme::MUTED));
                theme::meter(ui, 56.0, frac, theme::load_color(frac));
                ui.label(
                    egui::RichText::new(format!(
                        "{} / {}",
                        format::bytes(used),
                        format::bytes(r.mem_total)
                    ))
                    .small(),
                );
            }
            for g in &r.gpus {
                if let Some(u) = g.utilization {
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new("GPU").small().color(theme::MUTED));
                    theme::meter(ui, 56.0, u / 100.0, theme::load_color(u / 100.0));
                    ui.label(egui::RichText::new(format!("{u:.0} %")).small());
                }
            }
            if let Some(gate) = &self.snap.gate {
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(format!("⏳ {}", gate.describe()))
                        .small()
                        .color(theme::WARN),
                );
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let Some(n) = self.notices.back() else {
                    return;
                };
                if n.at.elapsed() >= Duration::from_secs(30) && n.level != NoticeLevel::Error {
                    return;
                }
                let (icon, color) = match n.level {
                    NoticeLevel::Info => ("ℹ", theme::TEXT),
                    NoticeLevel::Warning => ("⚠", theme::WARN),
                    NoticeLevel::Error => ("✖", theme::ERROR),
                };
                // offered only while the job is still there and retryable
                let retry = n.job.filter(|id| {
                    self.snap
                        .jobs
                        .iter()
                        .any(|j| j.id == *id && j.state.can_retry())
                });
                if let Some(id) = retry
                    && ui
                        .small_button(tr!("↻ Retry"))
                        .on_hover_text(tr!(
                            "Put the job back on the queue – it starts from the beginning"
                        ))
                        .clicked()
                {
                    self.send(Command::RetryJob(id));
                }
                ui.add(
                    egui::Label::new(egui::RichText::new(&n.text).small().color(color)).truncate(),
                )
                .on_hover_text(&n.text);
                ui.label(egui::RichText::new(icon).color(color));
            });
        });
    }
}

/// One sidebar entry: icon, label, optional count; highlighted when active.
fn nav_item(
    ui: &mut egui::Ui,
    icon: &str,
    label: &str,
    selected: bool,
    badge: Option<usize>,
) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 34.0), egui::Sense::click());
    let hovered = response.hovered();
    let painter = ui.painter();
    if selected {
        painter.rect_filled(rect, 7.0, theme::ACCENT.gamma_multiply(0.16));
        painter.rect_filled(
            egui::Rect::from_min_size(
                egui::pos2(rect.left(), rect.top() + 8.0),
                egui::vec2(3.0, rect.height() - 16.0),
            ),
            1.5,
            theme::ACCENT,
        );
    } else if hovered {
        painter.rect_filled(rect, 7.0, theme::SURFACE);
    }
    let color = if selected {
        theme::TEXT_STRONG
    } else if hovered {
        theme::TEXT
    } else {
        theme::MUTED
    };
    let y = rect.center().y;
    painter.text(
        egui::pos2(rect.left() + 14.0, y),
        egui::Align2::LEFT_CENTER,
        icon,
        egui::FontId::proportional(15.0),
        if selected { theme::ACCENT } else { color },
    );
    painter.text(
        egui::pos2(rect.left() + 40.0, y),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(14.0),
        color,
    );
    if let Some(n) = badge {
        let text = n.to_string();
        let galley =
            painter.layout_no_wrap(text, egui::FontId::proportional(11.0), theme::ON_ACCENT);
        let size = egui::vec2((galley.size().x + 12.0).max(20.0), 18.0);
        let pill =
            egui::Rect::from_center_size(egui::pos2(rect.right() - 10.0 - size.x / 2.0, y), size);
        painter.rect_filled(pill, 9.0, theme::ACCENT);
        painter.galley(
            pill.center() - galley.size() / 2.0,
            galley,
            theme::ON_ACCENT,
        );
    }
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
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

        let chrome = |margin: egui::Margin| {
            egui::Frame::new()
                .fill(theme::CHROME)
                .inner_margin(margin)
                .stroke(egui::Stroke::new(1.0, theme::BORDER))
        };
        egui::Panel::left("nav")
            .resizable(false)
            .exact_size(212.0)
            .frame(chrome(egui::Margin::symmetric(10, 0)))
            .show(ui, |ui| self.nav(ui));
        egui::Panel::top("top_bar")
            .frame(chrome(egui::Margin::symmetric(20, 10)))
            .show(ui, |ui| self.top_bar(ui));
        egui::Panel::bottom("status_bar")
            .frame(chrome(egui::Margin::symmetric(16, 5)))
            .show(ui, |ui| self.status_bar(ui));
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::BG)
                    .inner_margin(egui::Margin::symmetric(24, 18)),
            )
            .show(ui, |ui| match self.view {
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
