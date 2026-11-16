//! Messages between GUI and backend.
//!
//! The GUI sends [`Command`]s and renders snapshots received as [`AppEvent`]s;
//! it never touches domain state directly.

use std::path::PathBuf;
use std::sync::Arc;

use super::config::{AppConfig, JellyfinSettings};
use super::paths::{ConflictResolution, ConflictStrategy};
use super::tools::ToolInfo;
use crate::analyzer::recommend::{Analysis, Recommendation};
use crate::encoding::hardware::HardwareCaps;
use crate::encoding::plan::{PlanBlocker, PlanWarning};
use crate::encoding::profile::EncodingProfile;
use crate::health::Health;
use crate::media::model::MediaFile;
use crate::monitor::{GateBlock, ResourceSnapshot};
use crate::provider::jellyfin::{QuickConnectEvent, ServerInfo};
use crate::queue::job::{HistoryEntry, Job, JobId};
use crate::scanner::{DuplicateGroup, ScanIssue, ScanProgress};

/// One row of the media library.
#[derive(Debug, Clone)]
pub struct LibraryEntry {
    pub media: MediaFile,
    pub analysis: Analysis,
    pub top_in_folder: bool,
    pub top_global: bool,
    pub duplicate: bool,
    pub health: Health,
    /// A job for this file exists in the queue (any state but finished).
    pub queued: bool,
}

pub type Library = Arc<Vec<LibraryEntry>>;

/// Job to be created after the user confirmed the preview.
#[derive(Debug, Clone)]
pub struct JobRequest {
    pub source: PathBuf,
    pub target: PathBuf,
    pub replace_existing: bool,
}

/// A failed history entry to put back on the queue.
#[derive(Debug, Clone)]
pub struct HistoryRequeue {
    pub job_id: JobId,
    pub source: PathBuf,
    pub target: PathBuf,
    pub profile_id: String,
}

#[derive(Debug, Clone)]
pub struct JobPreviewItem {
    pub source: PathBuf,
    pub relative: PathBuf,
    pub resolution: ConflictResolution,
    pub recommendation: Recommendation,
    pub warnings: Vec<PlanWarning>,
    pub blockers: Vec<PlanBlocker>,
    pub already_queued: bool,
}

#[derive(Debug, Clone)]
pub enum Command {
    UpdateConfig(Box<AppConfig>),
    StartScan,
    CancelScan,
    RescanHardware,
    /// Check the connection and list the libraries (answer: [`AppEvent::Jellyfin`]).
    TestJellyfin(JellyfinSettings),
    /// Log in with Quick Connect (answers: [`AppEvent::QuickConnect`]).
    QuickConnect {
        url: String,
        device_id: String,
    },
    CancelQuickConnect,
    /// Quick check (stage 2) of these files; `recheck` also repeats files
    /// whose current version was checked already.
    CheckHealth {
        paths: Vec<PathBuf>,
        recheck: bool,
    },
    CancelHealthCheck,
    PreviewJobs {
        paths: Vec<PathBuf>,
        profile_id: String,
        strategy: ConflictStrategy,
    },
    AddJobs {
        profile_id: String,
        items: Vec<JobRequest>,
    },
    SetQueuePaused(bool),
    PauseJob(JobId),
    ResumeJob(JobId),
    CancelJob(JobId),
    RetryJob(JobId),
    /// Queue every interrupted job again (after a restart or crash).
    RetryInterrupted,
    /// Put every failed job back on the queue.
    RetryFailed,
    /// Put failed history entries back on the queue (the job again if it
    /// still exists, otherwise a new one for the same source and target).
    RequeueHistory(Vec<HistoryRequeue>),
    RemoveJob(JobId),
    SetJobPriority(JobId, i32),
    ClearFinished,
    SaveProfile(Box<EncodingProfile>),
    DeleteProfile(String),
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone)]
pub enum AppEvent {
    Tools(ToolInfo),
    Hardware(Arc<HardwareCaps>),
    Config(Box<AppConfig>),
    ScanProgress(ScanProgress),
    ScanFinished {
        files: usize,
        reused: usize,
        issues: Vec<ScanIssue>,
        duplicates: Vec<DuplicateGroup>,
        cancelled: bool,
    },
    Library(Library),
    Jobs(Vec<Job>),
    JobUpdated(Box<Job>),
    QueueState {
        paused: bool,
        gate: Option<GateBlock>,
    },
    Resources(Box<ResourceSnapshot>),
    Profiles(Vec<EncodingProfile>),
    History(Vec<HistoryEntry>),
    JobPreview {
        profile_id: String,
        items: Vec<JobPreviewItem>,
    },
    Notice {
        level: NoticeLevel,
        text: String,
    },
    /// Result of [`Command::TestJellyfin`].
    Jellyfin(Result<ServerInfo, String>),
    QuickConnect(QuickConnectEvent),
    /// Running quick check (`None` = none running).
    HealthProgress(Option<HealthProgress>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct HealthProgress {
    pub done: usize,
    pub total: usize,
    pub defect: usize,
    pub suspect: usize,
    pub current: String,
}
