//! Backend: owns the domain state, executes commands and schedules jobs.
//!
//! Runs on its own Tokio runtime thread. All long operations (scans, ffmpeg,
//! hardware tests) run as tasks and report back through an internal channel,
//! so the command loop – and therefore the GUI – never blocks.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;

use super::config::{AppConfig, AppDirs};
use super::desktop_notify;
use super::events::{
    AppEvent, Command, JobPreviewItem, JobRequest, Library, LibraryEntry, NoticeLevel,
};
use super::paths::{self, ConflictResolution, Roots};
use super::tools::{self, ToolInfo, ToolPaths};
use crate::analyzer::calibration::SizeCalibration;
use crate::analyzer::recommend;
use crate::encoding::hardware::{self, HardwareCaps};
use crate::encoding::os::CAN_SUSPEND;
use crate::encoding::plan::{self, PlanCapabilities};
use crate::encoding::process::ProcessHandle;
use crate::encoding::profile::EncodingProfile;
use crate::i18n;
use crate::media::model::MediaFile;
use crate::monitor::{self, GateBlock, Monitor, ResourceSnapshot};
use crate::persistence::Database;
use crate::queue::job::{self, HistoryEntry, Job, JobId, JobState, now_unix};
use crate::queue::runner::{self, RunOutcome, RunnerContext, RunnerEvent};
use crate::scanner::{self, DiscoveryOptions, DuplicateGroup, ScanOutcome, ScanRequest};
use crate::{tr, trf};

/// Callback used to wake up the GUI after an event was sent.
pub type Notify = Arc<dyn Fn() + Send + Sync>;

/// Handle owned by the GUI.
pub struct BackendHandle {
    pub commands: mpsc::UnboundedSender<Command>,
    pub events: std::sync::mpsc::Receiver<AppEvent>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl BackendHandle {
    pub fn send(&self, cmd: Command) {
        if self.commands.send(cmd).is_err() {
            tracing::error!("backend is not running");
        }
    }

    /// Stop running jobs (marked as interrupted) and wait for the backend.
    pub fn shutdown(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

enum Internal {
    Tools(ToolInfo),
    Hardware(HardwareCaps),
    Resources(ResourceSnapshot),
    ScanProgress(scanner::ScanProgress),
    ScanDone(Roots, ScanOutcome),
    Job(JobId, RunnerEvent),
    JobDone(JobId, RunOutcome),
}

struct Backend {
    dirs: AppDirs,
    config: AppConfig,
    db: Database,
    events: std::sync::mpsc::Sender<AppEvent>,
    notify: Notify,
    internal: mpsc::UnboundedSender<Internal>,

    caps: Option<Arc<HardwareCaps>>,
    profiles: Vec<EncodingProfile>,
    media: Vec<MediaFile>,
    duplicates: Vec<DuplicateGroup>,
    jobs: Vec<Job>,
    running: HashMap<JobId, ProcessHandle>,
    next_job_id: JobId,
    queue_paused: bool,
    gate: Option<GateBlock>,
    snapshot: ResourceSnapshot,
    scan_cancel: Option<Arc<AtomicBool>>,
    /// Size-estimate correction from finished jobs.
    calibration: SizeCalibration,
}

/// Start the backend thread.
pub fn spawn(dirs: AppDirs, config: AppConfig, db: Database, notify: Notify) -> BackendHandle {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (ev_tx, ev_rx) = std::sync::mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("framewise-backend".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_name("framewise-worker")
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!("cannot start async runtime: {e}");
                    return;
                }
            };
            runtime.block_on(run(dirs, config, db, cmd_rx, ev_tx, notify));
            runtime.shutdown_timeout(Duration::from_secs(5));
        })
        .expect("spawn backend thread");
    BackendHandle {
        commands: cmd_tx,
        events: ev_rx,
        thread: Some(thread),
    }
}

async fn run(
    dirs: AppDirs,
    config: AppConfig,
    db: Database,
    mut commands: mpsc::UnboundedReceiver<Command>,
    events: std::sync::mpsc::Sender<AppEvent>,
    notify: Notify,
) {
    let (internal_tx, mut internal_rx) = mpsc::unbounded_channel();
    let mut backend = Backend::new(dirs, config, db, events, notify, internal_tx);
    backend.startup();

    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            cmd = commands.recv() => match cmd {
                Some(Command::Shutdown) | None => {
                    backend.shutdown().await;
                    break;
                }
                Some(cmd) => backend.handle(cmd),
            },
            Some(msg) = internal_rx.recv() => backend.handle_internal(msg),
            _ = tick.tick() => backend.schedule(),
        }
    }
}

impl Backend {
    fn new(
        dirs: AppDirs,
        config: AppConfig,
        db: Database,
        events: std::sync::mpsc::Sender<AppEvent>,
        notify: Notify,
        internal: mpsc::UnboundedSender<Internal>,
    ) -> Self {
        Self {
            dirs,
            config,
            db,
            events,
            notify,
            internal,
            caps: None,
            profiles: EncodingProfile::builtins(),
            media: Vec::new(),
            duplicates: Vec::new(),
            jobs: Vec::new(),
            running: HashMap::new(),
            next_job_id: 1,
            queue_paused: false,
            gate: None,
            snapshot: ResourceSnapshot::default(),
            scan_cancel: None,
            calibration: SizeCalibration::default(),
        }
    }

    // ---------------------------------------------------------------- helpers

    fn emit(&self, event: AppEvent) {
        if self.events.send(event).is_ok() {
            (self.notify)();
        }
    }

    fn notice(&self, level: NoticeLevel, text: impl Into<String>) {
        self.emit(AppEvent::Notice {
            level,
            text: text.into(),
        });
    }

    fn tool_paths(&self) -> ToolPaths {
        ToolPaths {
            ffmpeg: self.config.ffmpeg_path.clone(),
            ffprobe: self.config.ffprobe_path.clone(),
        }
    }

    fn work_root(&self) -> PathBuf {
        self.config.work_dir(&self.dirs)
    }

    fn plan_caps(&self) -> PlanCapabilities {
        self.caps
            .as_ref()
            .map(|c| c.plan_capabilities())
            .unwrap_or_default()
    }

    fn profile(&self, id: &str) -> Option<&EncodingProfile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    fn active_profile(&self) -> EncodingProfile {
        self.profile(&self.config.encoding.active_profile)
            .cloned()
            .unwrap_or_else(EncodingProfile::universal)
    }

    fn roots(&self) -> super::error::Result<Roots> {
        match (&self.config.source_dir, &self.config.target_dir) {
            (Some(s), Some(t)) => paths::validate_roots(s, t),
            _ => Err(super::error::Error::InvalidPaths(
                tr!("Source and target directory are required").into(),
            )),
        }
    }

    fn save_job(&self, job: &Job) {
        if let Err(e) = self.db.save_job(job) {
            tracing::error!("saving job {} failed: {e}", job.id);
        }
    }

    fn send_jobs(&self) {
        self.emit(AppEvent::Jobs(self.jobs.clone()));
    }

    fn send_queue_state(&self) {
        self.emit(AppEvent::QueueState {
            paused: self.queue_paused,
            gate: self.gate.clone(),
        });
    }

    fn send_history(&mut self) {
        match self.db.load_history(2000) {
            Ok(h) => {
                self.calibration = SizeCalibration::from_history(&h);
                self.emit(AppEvent::History(h));
            }
            Err(e) => tracing::error!("loading history failed: {e}"),
        }
    }

    // --------------------------------------------------------------- startup

    fn startup(&mut self) {
        self.emit(AppEvent::Config(Box::new(self.config.clone())));
        match self.db.load_profiles() {
            Ok(p) => self.profiles = p,
            Err(e) => self.notice(NoticeLevel::Error, e.to_string()),
        }
        self.emit(AppEvent::Profiles(self.profiles.clone()));

        // Job recovery.
        match self.db.load_jobs() {
            Ok(mut jobs) => {
                let n = job::recover(&mut jobs);
                for j in jobs.iter().filter(|j| j.state == JobState::Interrupted) {
                    self.save_job(j);
                }
                if n > 0 {
                    self.notice(
                        NoticeLevel::Warning,
                        trf!("{} interrupted job(s) recovered – they can be restarted", n),
                    );
                }
                self.jobs = jobs;
            }
            Err(e) => self.notice(NoticeLevel::Error, e.to_string()),
        }
        self.next_job_id = self.db.next_job_id().unwrap_or(1);
        runner::cleanup_orphans(&self.work_root(), &[]);
        // Queued jobs are not started automatically after a restart.
        self.queue_paused = self.jobs.iter().any(|j| j.state == JobState::Queued);
        self.send_jobs();
        self.send_queue_state();
        self.send_history();

        // Cached scan of the configured source.
        if let Ok(roots) = self.roots()
            && let Ok(cache) = self.db.load_media_cache(&roots.source)
        {
            let mut media: Vec<MediaFile> = cache.into_values().collect();
            media.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
            self.media = media;
        }
        self.publish_library();

        self.detect_tools();
        self.start_monitor();
    }

    fn detect_tools(&self) {
        let paths = self.tool_paths();
        let vaapi = self.config.resources.vaapi_device.clone();
        let tx = self.internal.clone();
        tokio::spawn(async move {
            let info = tools::detect(&paths).await;
            let ready = info.ffmpeg_version.is_some();
            let _ = tx.send(Internal::Tools(info));
            if ready {
                let caps = hardware::scan(&paths.ffmpeg, &vaapi).await;
                let _ = tx.send(Internal::Hardware(caps));
            }
        });
    }

    fn start_monitor(&self) {
        let tx = self.internal.clone();
        tokio::spawn(async move {
            let mut monitor = Monitor::new();
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let snap = monitor.sample().await;
                if tx.send(Internal::Resources(snap)).is_err() {
                    break;
                }
            }
        });
    }

    // --------------------------------------------------------------- library

    fn publish_library(&self) {
        let profile = self.active_profile();
        let caps = self.plan_caps();
        let top_n = self.config.scan.top_n_per_folder;
        let top_folder: HashSet<usize> = scanner::ranking::top_n_per_folder(&self.media, top_n)
            .into_iter()
            .collect();
        let top_global: HashSet<usize> =
            scanner::ranking::top_n_global(&self.media, (self.media.len() / 10).clamp(1, 25))
                .into_iter()
                .collect();
        let dup_paths: HashSet<&Path> = self
            .duplicates
            .iter()
            .flat_map(|g| g.paths.iter().map(PathBuf::as_path))
            .collect();
        let queued: HashSet<&Path> = self
            .jobs
            .iter()
            .filter(|j| !j.state.is_finished())
            .map(|j| j.media.absolute_path.as_path())
            .collect();
        let entries: Vec<LibraryEntry> = self
            .media
            .iter()
            .enumerate()
            .map(|(i, m)| LibraryEntry {
                analysis: recommend::analyze_calibrated(m, &profile, &caps, &self.calibration),
                top_in_folder: top_folder.contains(&i),
                top_global: top_global.contains(&i),
                duplicate: dup_paths.contains(m.absolute_path.as_path()),
                queued: queued.contains(m.absolute_path.as_path()),
                media: m.clone(),
            })
            .collect();
        let library: Library = Arc::new(entries);
        self.emit(AppEvent::Library(library));
    }

    // -------------------------------------------------------------- commands

    fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::UpdateConfig(cfg) => self.update_config(*cfg),
            Command::StartScan => self.start_scan(),
            Command::CancelScan => {
                if let Some(c) = &self.scan_cancel {
                    c.store(true, Ordering::Relaxed);
                }
            }
            Command::RescanHardware => self.detect_tools(),
            Command::PreviewJobs {
                paths,
                profile_id,
                strategy,
            } => self.preview_jobs(&paths, profile_id, strategy),
            Command::AddJobs { profile_id, items } => self.add_jobs(&profile_id, items),
            Command::SetQueuePaused(p) => {
                self.queue_paused = p;
                self.send_queue_state();
            }
            Command::PauseJob(id) => self.pause_job(id, true),
            Command::ResumeJob(id) => self.pause_job(id, false),
            Command::CancelJob(id) => self.cancel_job(id),
            Command::RetryJob(id) => {
                if let Some(j) = self
                    .jobs
                    .iter_mut()
                    .find(|j| j.id == id && j.state.can_retry())
                {
                    j.reset_for_retry();
                    let j = j.clone();
                    self.save_job(&j);
                    self.emit(AppEvent::JobUpdated(Box::new(j)));
                }
            }
            Command::RemoveJob(id) => {
                if let Some(pos) = self
                    .jobs
                    .iter()
                    .position(|j| j.id == id && !j.state.is_active())
                {
                    self.jobs.remove(pos);
                    let _ = self.db.delete_job(id);
                    self.send_jobs();
                    self.publish_library();
                }
            }
            Command::SetJobPriority(id, priority) => {
                if let Some(j) = self.jobs.iter_mut().find(|j| j.id == id) {
                    j.priority = priority;
                    let j = j.clone();
                    self.save_job(&j);
                    self.emit(AppEvent::JobUpdated(Box::new(j)));
                }
            }
            Command::ClearFinished => {
                let finished: Vec<JobId> = self
                    .jobs
                    .iter()
                    .filter(|j| {
                        matches!(
                            j.state,
                            JobState::Done | JobState::Skipped | JobState::Cancelled
                        )
                    })
                    .map(|j| j.id)
                    .collect();
                for id in &finished {
                    let _ = self.db.delete_job(*id);
                }
                self.jobs.retain(|j| !finished.contains(&j.id));
                self.send_jobs();
                self.publish_library();
            }
            Command::SaveProfile(p) => match self.db.save_profile(&p) {
                Ok(()) => {
                    match self.profiles.iter_mut().find(|x| x.id == p.id) {
                        Some(slot) => *slot = *p,
                        None => self.profiles.push(*p),
                    }
                    self.emit(AppEvent::Profiles(self.profiles.clone()));
                    self.publish_library();
                }
                Err(e) => self.notice(NoticeLevel::Error, e.to_string()),
            },
            Command::DeleteProfile(id) => {
                if self.profiles.iter().any(|p| p.id == id && p.builtin) {
                    // Built-ins can only be reset to defaults.
                    let _ = self.db.delete_profile(&id);
                    self.profiles = self
                        .db
                        .load_profiles()
                        .unwrap_or_else(|_| EncodingProfile::builtins());
                } else if self.db.delete_profile(&id).is_ok() {
                    self.profiles.retain(|p| p.id != id);
                }
                if self.config.encoding.active_profile == id
                    && !self.profiles.iter().any(|p| p.id == id)
                {
                    self.config.encoding.active_profile = "universal".into();
                    let _ = self.config.save(&self.dirs.config_file);
                    self.emit(AppEvent::Config(Box::new(self.config.clone())));
                }
                self.emit(AppEvent::Profiles(self.profiles.clone()));
                self.publish_library();
            }
            Command::Shutdown => {}
        }
    }

    fn update_config(&mut self, cfg: AppConfig) {
        let tools_changed = cfg.ffmpeg_path != self.config.ffmpeg_path
            || cfg.ffprobe_path != self.config.ffprobe_path
            || cfg.resources.vaapi_device != self.config.resources.vaapi_device;
        let library_changed = cfg.encoding.active_profile != self.config.encoding.active_profile
            || cfg.scan.top_n_per_folder != self.config.scan.top_n_per_folder;
        let source_changed = cfg.source_dir != self.config.source_dir;
        self.config = cfg;
        i18n::set_language(
            self.config
                .ui
                .language
                .unwrap_or_else(i18n::Language::from_env),
        );
        if let Err(e) = self.config.save(&self.dirs.config_file) {
            self.notice(NoticeLevel::Error, e.to_string());
        }
        if tools_changed {
            self.detect_tools();
        }
        if source_changed {
            self.media.clear();
            self.duplicates.clear();
            if let Ok(roots) = self.roots()
                && let Ok(cache) = self.db.load_media_cache(&roots.source)
            {
                self.media = cache.into_values().collect();
                self.media
                    .sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
            }
        }
        if library_changed || source_changed {
            self.publish_library();
        }
        self.emit(AppEvent::Config(Box::new(self.config.clone())));
    }

    fn start_scan(&mut self) {
        if self.scan_cancel.is_some() {
            return;
        }
        let roots = match self.roots() {
            Ok(r) => r,
            Err(e) => {
                self.notice(NoticeLevel::Error, e.to_string());
                return;
            }
        };
        let mut exclude = vec![roots.target.clone()];
        if let Ok(w) = paths::normalize_path(&self.work_root()) {
            exclude.push(w);
        }
        let request = ScanRequest {
            discovery: DiscoveryOptions {
                root: roots.source.clone(),
                extensions: self.config.scan.extensions.clone(),
                follow_symlinks: self.config.scan.follow_symlinks,
                skip_hidden: self.config.scan.skip_hidden,
                exclude,
            },
            ffprobe: self.config.ffprobe_path.clone(),
            parallelism: self.config.probe_parallelism(),
            detect_duplicates: self.config.scan.detect_duplicates,
        };
        let cache = self.db.load_media_cache(&roots.source).unwrap_or_default();
        let cancel = Arc::new(AtomicBool::new(false));
        self.scan_cancel = Some(cancel.clone());
        let tx = self.internal.clone();
        tokio::spawn(async move {
            let ptx = tx.clone();
            let outcome = scanner::run_scan(request, cache, cancel, move |p| {
                let _ = ptx.send(Internal::ScanProgress(p));
            })
            .await;
            let _ = tx.send(Internal::ScanDone(roots, outcome));
        });
    }

    fn reserved_targets(&self) -> HashSet<PathBuf> {
        self.jobs
            .iter()
            .filter(|j| !j.state.is_finished())
            .map(|j| j.target.clone())
            .collect()
    }

    fn preview_jobs(
        &mut self,
        paths: &[PathBuf],
        profile_id: String,
        strategy: paths::ConflictStrategy,
    ) {
        let roots = match self.roots() {
            Ok(r) => r,
            Err(e) => {
                self.notice(NoticeLevel::Error, e.to_string());
                return;
            }
        };
        let Some(profile) = self.profile(&profile_id).cloned() else {
            self.notice(
                NoticeLevel::Error,
                trf!("Profile not found: {}", profile_id),
            );
            return;
        };
        let caps = self.plan_caps();
        let mut reserved = self.reserved_targets();
        let queued_sources: HashSet<&Path> = self
            .jobs
            .iter()
            .filter(|j| !j.state.is_finished())
            .map(|j| j.media.absolute_path.as_path())
            .collect();
        let mut items = Vec::new();
        for path in paths {
            let Some(m) = self.media.iter().find(|m| &m.absolute_path == path) else {
                continue;
            };
            let analysis = recommend::analyze_calibrated(m, &profile, &caps, &self.calibration);
            let desired = paths::target_for_relative(&roots.target, &m.relative_path, "mp4");
            let resolution =
                paths::resolve_conflict(&desired, strategy, &|p: &Path| p.exists(), &|p: &Path| {
                    reserved.contains(p)
                });
            if let Some(p) = resolution.path() {
                reserved.insert(p.to_path_buf());
            }
            items.push(JobPreviewItem {
                source: m.absolute_path.clone(),
                relative: m.relative_path.clone(),
                resolution,
                recommendation: analysis.recommendation,
                warnings: analysis.plan.warnings.clone(),
                blockers: analysis.plan.blockers.clone(),
                already_queued: queued_sources.contains(m.absolute_path.as_path()),
            });
        }
        self.emit(AppEvent::JobPreview { profile_id, items });
    }

    fn add_jobs(&mut self, profile_id: &str, items: Vec<JobRequest>) {
        let Some(profile) = self.profile(profile_id).cloned() else {
            return;
        };
        let caps = self.plan_caps();
        let reserved = self.reserved_targets();
        let mut added = 0;
        for req in items {
            if reserved.contains(&req.target) {
                continue;
            }
            let Some(m) = self.media.iter().find(|m| m.absolute_path == req.source) else {
                continue;
            };
            let mut plan = plan::build_plan(m, &profile, &caps);
            self.calibration.apply(m, &mut plan);
            if plan.is_blocked() {
                continue;
            }
            let mut job = Job::new(
                self.next_job_id,
                m.clone(),
                req.target,
                profile.id.clone(),
                plan,
            );
            self.next_job_id += 1;
            job.replace_existing = req.replace_existing;
            self.save_job(&job);
            self.jobs.push(job);
            added += 1;
        }
        self.send_jobs();
        self.publish_library();
        self.notice(
            NoticeLevel::Info,
            trf!("{} job(s) added to the queue", added),
        );
    }

    fn pause_job(&mut self, id: JobId, pause: bool) {
        let Some(handle) = self.running.get(&id) else {
            return;
        };
        if !CAN_SUSPEND {
            self.notice(
                NoticeLevel::Warning,
                tr!("Pausing a running job is not supported on this system – pause the queue instead"),
            );
            return;
        }
        let ok = if pause {
            handle.pause()
        } else {
            handle.resume()
        };
        if let Some(j) = self.jobs.iter_mut().find(|j| j.id == id) {
            j.paused = pause && ok;
            let j = j.clone();
            self.emit(AppEvent::JobUpdated(Box::new(j)));
        }
    }

    fn cancel_job(&mut self, id: JobId) {
        if let Some(handle) = self.running.get(&id) {
            handle.cancel();
            return;
        }
        if let Some(j) = self
            .jobs
            .iter_mut()
            .find(|j| j.id == id && matches!(j.state, JobState::Queued | JobState::Interrupted))
        {
            j.state = JobState::Cancelled;
            j.finished_unix = Some(now_unix());
            let j = j.clone();
            self.save_job(&j);
            self.emit(AppEvent::JobUpdated(Box::new(j)));
            self.publish_library();
        }
    }

    // ------------------------------------------------------------- internal

    fn handle_internal(&mut self, msg: Internal) {
        match msg {
            Internal::Tools(info) => {
                if !info.ready() {
                    self.notice(
                        NoticeLevel::Error,
                        tr!("ffmpeg/ffprobe not found – please check the paths in the settings"),
                    );
                }
                self.emit(AppEvent::Tools(info));
            }
            Internal::Hardware(caps) => {
                let caps = Arc::new(caps);
                self.caps = Some(caps.clone());
                self.emit(AppEvent::Hardware(caps));
                self.publish_library();
            }
            Internal::Resources(snap) => {
                self.snapshot = snap.clone();
                self.emit(AppEvent::Resources(Box::new(snap)));
            }
            Internal::ScanProgress(p) => self.emit(AppEvent::ScanProgress(p)),
            Internal::ScanDone(roots, outcome) => {
                self.scan_cancel = None;
                if !outcome.cancelled {
                    self.media = outcome.files;
                    self.duplicates = outcome.duplicates.clone();
                    let db = self.db.clone();
                    let media = self.media.clone();
                    let root = roots.source.clone();
                    tokio::task::spawn_blocking(move || {
                        if let Err(e) = db.replace_media(&root, &media) {
                            tracing::error!("storing scan failed: {e}");
                        }
                    });
                    self.publish_library();
                }
                self.emit(AppEvent::ScanFinished {
                    files: self.media.len(),
                    reused: outcome.reused,
                    issues: outcome.issues,
                    duplicates: outcome.duplicates,
                    cancelled: outcome.cancelled,
                });
            }
            Internal::Job(id, event) => self.apply_job_event(id, event),
            Internal::JobDone(id, outcome) => self.finish_job(id, outcome),
        }
    }

    fn apply_job_event(&mut self, id: JobId, event: RunnerEvent) {
        let Some(job) = self.jobs.iter_mut().find(|j| j.id == id) else {
            return;
        };
        let persist = !matches!(event, RunnerEvent::Progress(_));
        match event {
            RunnerEvent::State(s) => {
                job.state = s;
                job.progress = Default::default();
            }
            RunnerEvent::Progress(p) => job.progress = p,
            RunnerEvent::Encoder { label, fallback } => {
                job.encoder = Some(label);
                job.used_fallback |= fallback;
            }
            RunnerEvent::Replanned(plan) => job.plan = *plan,
            RunnerEvent::LogFile(p) => job.log_file = Some(p),
        }
        let job = job.clone();
        if persist {
            self.save_job(&job);
        }
        self.emit(AppEvent::JobUpdated(Box::new(job)));
    }

    fn finish_job(&mut self, id: JobId, outcome: RunOutcome) {
        self.running.remove(&id);
        let Some(job) = self.jobs.iter_mut().find(|j| j.id == id) else {
            return;
        };
        job.state = outcome.state.unwrap_or(JobState::Failed);
        job.error = outcome.error;
        job.result_size = outcome.result_size;
        job.quality = outcome.quality;
        job.validation = outcome.validation;
        job.sidecars = outcome.sidecars;
        job.finished_unix = Some(now_unix());
        job.paused = false;
        job.progress = Default::default();
        let job = job.clone();
        self.save_job(&job);
        if job.state != JobState::Cancelled
            && let Err(e) = self.db.add_history(&HistoryEntry::from_job(&job))
        {
            tracing::error!("history insert failed: {e}");
        }
        match job.state {
            JobState::Failed => self.notice(
                NoticeLevel::Error,
                trf!(
                    "Job failed: {} – {}",
                    job.media.file_name,
                    job.error.clone().unwrap_or_default()
                ),
            ),
            JobState::Done => self.notice(
                NoticeLevel::Info,
                trf!("Job finished: {}", job.media.file_name),
            ),
            _ => {}
        }
        self.notify_desktop(&job);
        self.emit(AppEvent::JobUpdated(Box::new(job)));
        self.send_history();
        self.publish_library();
    }

    /// Desktop notification for a finished job and, once nothing is left to
    /// run, for the whole queue.
    fn notify_desktop(&self, job: &Job) {
        if !self.config.ui.desktop_notifications {
            return;
        }
        match job.state {
            JobState::Done => {
                let sizes = match job.result_size {
                    Some(r) => format!(
                        "{} → {}",
                        crate::gui::format::bytes(job.media.size_bytes),
                        crate::gui::format::bytes(r)
                    ),
                    None => String::new(),
                };
                desktop_notify::send(trf!("Job finished: {}", job.media.file_name), sizes);
            }
            JobState::Failed => desktop_notify::send(
                trf!("Job failed: {}", job.media.file_name),
                job.error.clone().unwrap_or_default(),
            ),
            _ => return,
        }
        let pending = self
            .jobs
            .iter()
            .any(|j| j.state == JobState::Queued || j.state.is_active());
        if !pending {
            let count = |st: JobState| self.jobs.iter().filter(|j| j.state == st).count();
            desktop_notify::send(
                tr!("Queue finished").to_string(),
                trf!(
                    "{} done, {} failed",
                    count(JobState::Done),
                    count(JobState::Failed)
                ),
            );
        }
    }

    // ------------------------------------------------------------- scheduler

    fn schedule(&mut self) {
        let max = self.config.resources.max_parallel_jobs.max(1);
        let mut gate = None;
        while !self.queue_paused && self.running.len() < max {
            let Some(idx) = job::next_runnable(&self.jobs) else {
                break;
            };
            let mut paths = vec![self.work_root()];
            if let Some(t) = &self.config.target_dir {
                paths.push(t.clone());
            }
            gate = monitor::check_gate(
                &self.snapshot,
                &self.config.resources,
                &paths,
                self.running.len(),
            );
            if gate.is_some() {
                break;
            }
            self.start_job(idx);
        }
        if gate != self.gate {
            self.gate = gate;
            self.send_queue_state();
        }
    }

    fn start_job(&mut self, idx: usize) {
        let job = &mut self.jobs[idx];
        let Some(profile) = self
            .profiles
            .iter()
            .find(|p| p.id == job.profile_id)
            .cloned()
        else {
            job.state = JobState::Failed;
            job.error = Some(trf!("Profile not found: {}", job.profile_id));
            let j = job.clone();
            self.save_job(&j);
            self.emit(AppEvent::JobUpdated(Box::new(j)));
            return;
        };
        job.state = JobState::Preparing;
        job.attempts += 1;
        job.started_unix = Some(now_unix());
        job.finished_unix = None;
        let job = job.clone();
        self.save_job(&job);
        self.emit(AppEvent::JobUpdated(Box::new(job.clone())));

        let ctx = RunnerContext {
            tools: self.tool_paths(),
            work_root: self.work_root(),
            job_log_dir: self.dirs.job_log_dir(),
            caps: self.caps.as_deref().cloned().unwrap_or_default(),
            resources: self.config.resources.clone(),
            encoding: self.config.encoding.clone(),
            profile,
        };
        let handle = ProcessHandle::new();
        self.running.insert(job.id, handle.clone());
        let tx = self.internal.clone();
        tokio::spawn(async move {
            let id = job.id;
            let etx = tx.clone();
            let outcome = runner::run_job(&ctx, &job, &handle, move |ev| {
                let _ = etx.send(Internal::Job(id, ev));
            })
            .await;
            let _ = tx.send(Internal::JobDone(id, outcome));
        });
    }

    async fn shutdown(&mut self) {
        if let Some(c) = &self.scan_cancel {
            c.store(true, Ordering::Relaxed);
        }
        for handle in self.running.values() {
            handle.cancel();
        }
        // Give ffmpeg a moment to terminate, then persist the interrupted state.
        if !self.running.is_empty() {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        for job in self.jobs.iter_mut().filter(|j| j.state.is_active()) {
            job.state = JobState::Interrupted;
            job.progress = Default::default();
        }
        for job in self
            .jobs
            .iter()
            .filter(|j| j.state == JobState::Interrupted)
        {
            self.save_job(job);
        }
        runner::cleanup_orphans(&self.work_root(), &[]);
    }
}

/// Helper for tests and the GUI: is the resolution a replacement that needs
/// confirmation?
pub fn needs_confirmation(resolution: &ConflictResolution) -> bool {
    matches!(resolution, ConflictResolution::Replace(_))
}
