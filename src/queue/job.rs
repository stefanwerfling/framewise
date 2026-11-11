//! Job model of the encoding queue.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::analyzer::calibration::SizeSample;
use crate::encoding::plan::{EncodePlan, PlanWarning};
use crate::encoding::quality::QualityResult;
use crate::encoding::validate::ValidationReport;
use crate::media::model::MediaFile;
use crate::tr;

pub type JobId = i64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum JobState {
    Queued,
    Preparing,
    QualitySearch,
    Encoding,
    Sidecars,
    Validating,
    Finalizing,
    Done,
    Failed,
    Cancelled,
    Skipped,
    /// Was active when the application stopped; can be restarted.
    Interrupted,
}

impl JobState {
    pub fn is_active(self) -> bool {
        matches!(
            self,
            JobState::Preparing
                | JobState::QualitySearch
                | JobState::Encoding
                | JobState::Sidecars
                | JobState::Validating
                | JobState::Finalizing
        )
    }

    pub fn is_finished(self) -> bool {
        matches!(
            self,
            JobState::Done | JobState::Failed | JobState::Cancelled | JobState::Skipped
        )
    }

    pub fn can_retry(self) -> bool {
        matches!(
            self,
            JobState::Failed | JobState::Cancelled | JobState::Interrupted | JobState::Skipped
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            JobState::Queued => tr!("Queued"),
            JobState::Preparing => tr!("Preparing"),
            JobState::QualitySearch => tr!("Quality analysis"),
            JobState::Encoding => tr!("Encoding"),
            JobState::Sidecars => tr!("Exporting subtitles"),
            JobState::Validating => tr!("Validating"),
            JobState::Finalizing => tr!("Finalizing"),
            JobState::Done => tr!("Done"),
            JobState::Failed => tr!("Failed"),
            JobState::Cancelled => tr!("Cancelled"),
            JobState::Skipped => tr!("Skipped"),
            JobState::Interrupted => tr!("Interrupted"),
        }
    }

    pub fn as_db(self) -> &'static str {
        match self {
            JobState::Queued => "queued",
            JobState::Preparing => "preparing",
            JobState::QualitySearch => "quality",
            JobState::Encoding => "encoding",
            JobState::Sidecars => "sidecars",
            JobState::Validating => "validating",
            JobState::Finalizing => "finalizing",
            JobState::Done => "done",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
            JobState::Skipped => "skipped",
            JobState::Interrupted => "interrupted",
        }
    }
}

/// Live progress (not persisted in detail).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct JobProgress {
    /// 0..=1 of the current step.
    pub fraction: Option<f32>,
    pub speed: Option<f64>,
    pub fps: Option<f64>,
    pub eta_s: Option<f64>,
    pub out_time_s: Option<f64>,
    pub current_size: Option<u64>,
    /// Current CRF during quality search.
    pub probe_crf: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub id: JobId,
    pub media: MediaFile,
    pub target: PathBuf,
    /// Existing result may be replaced (explicitly confirmed by the user).
    pub replace_existing: bool,
    pub profile_id: String,
    pub plan: EncodePlan,
    /// Higher runs first; ties by `position`.
    pub priority: i32,
    pub position: i64,
    pub state: JobState,
    #[serde(default)]
    pub progress: JobProgress,
    pub paused: bool,
    /// e.g. `libx264` or `h264_nvenc + cuda`.
    pub encoder: Option<String>,
    pub used_fallback: bool,
    pub error: Option<String>,
    pub log_file: Option<PathBuf>,
    pub attempts: u32,
    pub created_unix: i64,
    pub started_unix: Option<i64>,
    pub finished_unix: Option<i64>,
    pub result_size: Option<u64>,
    pub quality: Option<QualityResult>,
    pub validation: Option<ValidationReport>,
    pub sidecars: Vec<PathBuf>,
}

impl Job {
    pub fn new(
        id: JobId,
        media: MediaFile,
        target: PathBuf,
        profile_id: String,
        plan: EncodePlan,
    ) -> Self {
        Self {
            id,
            media,
            target,
            replace_existing: false,
            profile_id,
            plan,
            priority: 0,
            position: id,
            state: JobState::Queued,
            progress: JobProgress::default(),
            paused: false,
            encoder: None,
            used_fallback: false,
            error: None,
            log_file: None,
            attempts: 0,
            created_unix: now_unix(),
            started_unix: None,
            finished_unix: None,
            result_size: None,
            quality: None,
            validation: None,
            sidecars: Vec::new(),
        }
    }

    pub fn elapsed_s(&self) -> Option<i64> {
        let start = self.started_unix?;
        Some(self.finished_unix.unwrap_or_else(now_unix) - start)
    }

    /// Reset for another attempt.
    pub fn reset_for_retry(&mut self) {
        self.state = JobState::Queued;
        self.progress = JobProgress::default();
        self.paused = false;
        self.error = None;
        self.started_unix = None;
        self.finished_unix = None;
        self.result_size = None;
        self.quality = None;
        self.validation = None;
        self.sidecars.clear();
        self.used_fallback = false;
    }
}

/// Finished job summary for the history view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: i64,
    pub job_id: JobId,
    pub source: PathBuf,
    pub target: PathBuf,
    pub source_size: u64,
    pub result_size: Option<u64>,
    pub profile_id: String,
    pub encoder: Option<String>,
    pub encode_seconds: Option<i64>,
    pub media_duration_s: Option<f64>,
    pub quality: Option<QualityResult>,
    pub validation_passed: bool,
    pub validation: Option<ValidationReport>,
    pub state: JobState,
    pub error: Option<String>,
    pub warnings: Vec<PlanWarning>,
    pub finished_unix: i64,
    /// Estimate vs. reality, feeds the size calibration (re-encodes only).
    #[serde(default)]
    pub size_sample: Option<SizeSample>,
}

impl HistoryEntry {
    pub fn from_job(job: &Job) -> Self {
        Self {
            id: 0,
            job_id: job.id,
            source: job.media.absolute_path.clone(),
            target: job.target.clone(),
            source_size: job.media.size_bytes,
            result_size: job.result_size,
            profile_id: job.profile_id.clone(),
            encoder: job.encoder.clone(),
            encode_seconds: job.elapsed_s(),
            media_duration_s: job.media.duration_s(),
            quality: job.quality.clone(),
            validation_passed: job
                .validation
                .as_ref()
                .is_some_and(ValidationReport::passed),
            validation: job.validation.clone(),
            state: job.state,
            error: job.error.clone(),
            warnings: job.plan.warnings.clone(),
            finished_unix: job.finished_unix.unwrap_or_else(now_unix),
            size_sample: SizeSample::from_plan(&job.media, &job.plan),
        }
    }

    /// Savings in percent (positive = smaller).
    pub fn savings_percent(&self) -> Option<f64> {
        let r = self.result_size?;
        (self.source_size > 0).then(|| (1.0 - r as f64 / self.source_size as f64) * 100.0)
    }
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Order in which queued jobs are started.
pub fn next_runnable(jobs: &[Job]) -> Option<usize> {
    jobs.iter()
        .enumerate()
        .filter(|(_, j)| j.state == JobState::Queued)
        .max_by(|(_, a), (_, b)| {
            a.priority
                .cmp(&b.priority)
                .then(b.position.cmp(&a.position))
        })
        .map(|(i, _)| i)
}

/// Estimated remaining wall-clock time of the queue (estimate!).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QueueEta {
    pub seconds: f64,
    /// Jobs included in the estimate.
    pub jobs: usize,
    /// Some jobs have no known duration and are not included.
    pub incomplete: bool,
}

/// Remaining time of active and queued jobs. The speed (media seconds per
/// wall-clock second) comes from the jobs encoding right now, otherwise from
/// recent history; without either no estimate is given.
pub fn queue_eta(jobs: &[Job], history: &[HistoryEntry], parallel: usize) -> Option<QueueEta> {
    let mut live: Vec<f64> = jobs
        .iter()
        .filter(|j| j.state == JobState::Encoding)
        .filter_map(|j| j.progress.speed)
        .filter(|s| s.is_finite() && *s > 0.0)
        .collect();
    let mut past: Vec<f64> = history
        .iter()
        .filter(|h| h.state == JobState::Done)
        .filter_map(|h| {
            let secs = h.encode_seconds.filter(|s| *s > 0)? as f64;
            Some(h.media_duration_s? / secs)
        })
        .filter(|s| s.is_finite() && *s > 0.0)
        .take(20)
        .collect();
    let speed = median(&mut live).or_else(|| median(&mut past))?;

    let mut per_job = Vec::new();
    let mut incomplete = false;
    for j in jobs {
        let remaining = match j.state {
            JobState::Encoding => j
                .progress
                .eta_s
                .or_else(|| j.media.duration_s().map(|d| d / speed)),
            JobState::Queued | JobState::Preparing | JobState::QualitySearch => {
                j.media.duration_s().map(|d| d / speed)
            }
            // Post-encode steps are short compared to the encode.
            JobState::Sidecars | JobState::Validating | JobState::Finalizing => Some(0.0),
            _ => continue,
        };
        match remaining {
            Some(r) if r.is_finite() => per_job.push(r.max(0.0)),
            _ => incomplete = true,
        }
    }
    if per_job.is_empty() {
        return None;
    }
    let sum: f64 = per_job.iter().sum();
    let longest = per_job.iter().copied().fold(0.0, f64::max);
    Some(QueueEta {
        seconds: (sum / parallel.max(1) as f64).max(longest),
        jobs: per_job.len(),
        incomplete,
    })
}

fn median(v: &mut [f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let n = v.len();
    Some(if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    })
}

/// Startup recovery: active jobs become `Interrupted`, queued stay queued.
pub fn recover(jobs: &mut [Job]) -> usize {
    let mut n = 0;
    for job in jobs.iter_mut().filter(|j| j.state.is_active()) {
        job.state = JobState::Interrupted;
        job.progress = JobProgress::default();
        job.paused = false;
        n += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::plan::{PlanCapabilities, build_plan};
    use crate::encoding::profile::EncodingProfile;
    use crate::media::fixtures::*;

    fn job(id: JobId) -> Job {
        let m = media(
            "mkv",
            1000,
            10.0,
            vec![video("h264", 1920, 1080)],
            vec![],
            vec![],
        );
        let plan = build_plan(
            &m,
            &EncodingProfile::universal(),
            &PlanCapabilities::default(),
        );
        Job::new(id, m, "/t/x.mp4".into(), "universal".into(), plan)
    }

    #[test]
    fn scheduling_order() {
        let mut jobs = vec![job(1), job(2), job(3)];
        assert_eq!(next_runnable(&jobs), Some(0), "FIFO");
        jobs[2].priority = 5;
        assert_eq!(next_runnable(&jobs), Some(2), "priority first");
        jobs[2].state = JobState::Encoding;
        jobs[0].state = JobState::Done;
        assert_eq!(next_runnable(&jobs), Some(1));
        jobs[1].state = JobState::Failed;
        assert_eq!(next_runnable(&jobs), None);
    }

    #[test]
    fn recovery_marks_active_jobs() {
        let mut jobs = vec![job(1), job(2), job(3)];
        jobs[0].state = JobState::Encoding;
        jobs[1].state = JobState::Validating;
        assert_eq!(recover(&mut jobs), 2);
        assert_eq!(jobs[0].state, JobState::Interrupted);
        assert_eq!(jobs[2].state, JobState::Queued);
        assert!(jobs[0].state.can_retry());
        jobs[0].reset_for_retry();
        assert_eq!(jobs[0].state, JobState::Queued);
    }

    #[test]
    fn queue_eta_uses_live_speed_then_history() {
        // Each fixture job is 10 s of media.
        let mut jobs = vec![job(1), job(2), job(3)];
        jobs[2].state = JobState::Done;
        // No speed known at all: no estimate.
        assert_eq!(queue_eta(&jobs, &[], 1), None);

        // History: 10 s media in 20 s -> 0.5x -> 20 s per queued job.
        let mut done = job(9);
        done.state = JobState::Done;
        done.started_unix = Some(100);
        done.finished_unix = Some(120);
        let hist = vec![HistoryEntry::from_job(&done)];
        let eta = queue_eta(&jobs, &hist, 1).unwrap();
        assert_eq!(eta.jobs, 2);
        assert!((eta.seconds - 40.0).abs() < 1e-9);

        // Live speed wins; the active job reports its own ETA.
        jobs[0].state = JobState::Encoding;
        jobs[0].progress.speed = Some(2.0);
        jobs[0].progress.eta_s = Some(3.0);
        let eta = queue_eta(&jobs, &hist, 1).unwrap();
        assert!((eta.seconds - 8.0).abs() < 1e-9, "3 s + 10 s / 2x");
        // Two parallel slots: never shorter than the longest single job.
        let eta = queue_eta(&jobs, &hist, 4).unwrap();
        assert!((eta.seconds - 5.0).abs() < 1e-9);
        assert!(!eta.incomplete);
    }
}
