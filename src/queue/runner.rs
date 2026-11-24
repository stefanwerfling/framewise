//! Executes one job: prepare → (quality search) → encode → sidecars →
//! validate → (quality check) → finalize.
//!
//! Safety guarantees:
//! * the source is only ever read,
//! * ffmpeg writes exclusively into `<work>/<job-id>/`,
//! * the result and its sidecars are published only after successful
//!   validation, as one transaction (see [`super::finalize`]),
//! * an existing target is never overwritten unless the user confirmed
//!   replacement for this job (no-clobber placement via hard links),
//! * on failure/cancel the job's work directory is removed (only that one).

use std::io;
use std::path::{Path, PathBuf};

use super::finalize::{self, FileRole, JobResult, PlannedFile, RealFs};
use super::job::{Job, JobProgress, JobState};
use super::method;
use crate::core::config::{EncodingSettings, ResourceSettings};
use crate::core::error::{Error, Result};
use crate::core::paths::numbered_variant;
use crate::core::tools::ToolPaths;
use crate::encoding::command::{self, EncodeCommand, EncoderSetup, Window};
use crate::encoding::crop;
use crate::encoding::hardware::HardwareCaps;
use crate::encoding::plan::{self, EncodePlan, PlanWarning, VideoAction};
use crate::encoding::process::{ProcessHandle, RunOptions, run_ffmpeg};
use crate::encoding::profile::{EncodingProfile, HdrStrategy, QualityMetric, TargetVideoCodec};
use crate::encoding::quality::{self, AutoChoice, QualityContext, QualityResult};
use crate::encoding::validate::{
    self, CheckKind, ValidateRequest, ValidationCheck, ValidationReport,
};
use crate::health::{self, Finding};
use crate::media::model::MediaFile;
use crate::media::probe::{self, FileFacts};
use crate::{tr, trf};

/// Everything a job needs from the application state (cloned per job).
#[derive(Debug, Clone)]
pub struct RunnerContext {
    pub tools: ToolPaths,
    pub work_root: PathBuf,
    pub job_log_dir: PathBuf,
    pub caps: HardwareCaps,
    pub resources: ResourceSettings,
    pub encoding: EncodingSettings,
    pub profile: EncodingProfile,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RunnerEvent {
    State(JobState),
    Progress(JobProgress),
    Encoder { label: String, fallback: bool },
    Replanned(Box<EncodePlan>),
    LogFile(PathBuf),
}

#[derive(Debug, Clone, Default)]
pub struct RunOutcome {
    pub state: Option<JobState>,
    pub error: Option<String>,
    pub result_size: Option<u64>,
    pub quality: Option<QualityResult>,
    pub validation: Option<ValidationReport>,
    pub sidecars: Vec<PathBuf>,
    /// Final target path (may differ for sidecar renames).
    pub target: Option<PathBuf>,
    /// Journal of the committed finalization. Pass it to
    /// [`finalize::release`] once the job state has been saved.
    pub journal: Option<PathBuf>,
}

impl RunOutcome {
    fn finished(state: JobState) -> Self {
        Self {
            state: Some(state),
            ..Self::default()
        }
    }
}

fn file_facts(media: &MediaFile) -> io::Result<FileFacts> {
    let meta = std::fs::metadata(&media.absolute_path)?;
    let modified_unix = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64);
    Ok(FileFacts {
        absolute_path: media.absolute_path.clone(),
        relative_path: media.relative_path.clone(),
        size_bytes: meta.len(),
        modified_unix,
    })
}

/// Encoder setup for `plan`. Keeping HDR always uses libx265 on the CPU:
/// only there are 10 bit, colour tags and mastering metadata guaranteed.
pub(super) fn resolve_setup(
    ctx: &RunnerContext,
    plan: &EncodePlan,
    codec: TargetVideoCodec,
) -> EncoderSetup {
    let r = &ctx.resources;
    let keeps_hdr = matches!(
        plan.video.as_ref().map(|v| &v.action),
        Some(VideoAction::Encode { hdr, .. }) if hdr.is_preserve()
    );
    EncoderSetup {
        hw_encode: if r.hw_encode_enabled && !keeps_hdr {
            ctx.caps.resolve_encoder(r.hw_encode, codec)
        } else {
            None
        },
        hw_decode: if r.hw_decode_enabled {
            ctx.caps.resolve_decoder(r.hw_decode)
        } else {
            None
        },
        threads: r.cpu_threads,
        vaapi_device: r.vaapi_device.clone(),
        gpu_index: r.gpu_index,
        x265_dolbyvision_opt: ctx.caps.libx265_dolbyvision,
    }
}

/// Remove a job work directory – only if it really lies inside `work_root`.
fn remove_work_dir(work_root: &Path, dir: &Path) {
    if dir.starts_with(work_root)
        && dir != work_root
        && dir.exists()
        && let Err(e) = std::fs::remove_dir_all(dir)
    {
        tracing::warn!("could not remove work dir {}: {e}", dir.display());
    }
}

pub fn job_work_dir(work_root: &Path, job_id: i64) -> PathBuf {
    work_root.join(format!("job-{job_id}"))
}

/// Remove work directories of jobs that are not running (startup recovery).
/// Jobs with a pending finalization journal keep theirs: recovery may still
/// need the files in it.
pub fn cleanup_orphans(work_root: &Path, active: &[i64]) {
    let Ok(entries) = std::fs::read_dir(work_root) else {
        return;
    };
    let pending: Vec<i64> = finalize::pending(work_root)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some(id) = name
            .strip_prefix("job-")
            .and_then(|n| n.parse::<i64>().ok())
            && !active.contains(&id)
            && !pending.contains(&id)
        {
            remove_work_dir(work_root, &e.path());
        }
    }
}

pub async fn run_job<F>(
    ctx: &RunnerContext,
    job: &Job,
    handle: &ProcessHandle,
    emit: F,
) -> RunOutcome
where
    F: Fn(RunnerEvent) + Send + Sync,
{
    if finalize::journal_path(&ctx.work_root, job.id).exists() {
        return RunOutcome {
            state: Some(JobState::Failed),
            error: Some(tr!("A previous finalization of this job is still pending – restart the application to complete or undo it").to_string()),
            ..RunOutcome::default()
        };
    }
    let work_dir = job_work_dir(&ctx.work_root, job.id);
    let result = run_inner(ctx, job, handle, &emit, &work_dir).await;
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(Error::Cancelled) => RunOutcome {
            state: Some(JobState::Cancelled),
            ..RunOutcome::default()
        },
        Err(err) => RunOutcome {
            state: Some(JobState::Failed),
            error: Some(err.to_string()),
            ..RunOutcome::default()
        },
    };
    remove_work_dir(&ctx.work_root, &work_dir);
    outcome
}

/// Fails with [`Error::SourceDamaged`] if the source contains zero-filled
/// regions. A failing scan itself is only logged: the encode and its
/// validation still catch real problems.
async fn check_source_intact(media: &MediaFile) -> Result<()> {
    let input = health::check::Input::File(media.absolute_path.clone());
    match health::check::zeroed_data(&input, media.duration_s()).await {
        Ok(findings) if findings.is_empty() => Ok(()),
        Ok(findings) => {
            let text: Vec<String> = findings.iter().map(Finding::describe).collect();
            Err(Error::SourceDamaged(text.join("; ")))
        }
        Err(e) => {
            tracing::warn!("zero scan of {} failed: {e}", media.absolute_path.display());
            Ok(())
        }
    }
}

/// Reads mastering display / content light level of an HDR source into
/// `media`. Without them HDR is still kept, only less precisely signalled.
async fn read_hdr_metadata(ctx: &RunnerContext, media: &mut MediaFile, plan: &EncodePlan) {
    let Some(vp) = plan.video.as_ref() else {
        return;
    };
    let path = media.absolute_path.clone();
    let Some(stream) = media
        .video
        .iter_mut()
        .find(|v| v.type_index == vp.source_type_index && !v.attached_pic)
    else {
        return;
    };
    if !stream.hdr.is_hdr() || stream.hdr_metadata.is_some() {
        return;
    }
    match probe::probe_hdr_metadata(&ctx.tools.ffprobe, &path, vp.source_type_index).await {
        Ok(meta) => {
            tracing::info!("HDR metadata: {meta:?}");
            stream.hdr_metadata = Some(meta);
        }
        Err(e) => tracing::warn!("HDR metadata not readable: {e}"),
    }
}

/// Detects black bars and stores them in `media`. Returns the new plan if
/// the bars change it. Detection errors are logged and ignored (full frame).
async fn detect_black_bars(
    ctx: &RunnerContext,
    media: &mut MediaFile,
    plan: &EncodePlan,
    handle: &ProcessHandle,
) -> Result<Option<EncodePlan>> {
    let Some(vp) = plan.video.as_ref() else {
        return Ok(None);
    };
    if !ctx.profile.video.auto_crop || plan.video_is_copied() {
        return Ok(None);
    }
    let Some(duration) = media.duration_s() else {
        return Ok(None);
    };
    let crop = match crop::detect(
        &ctx.tools.ffmpeg,
        &media.absolute_path,
        vp.source_type_index,
        vp.source,
        duration,
        handle,
    )
    .await
    {
        Ok(crop) => crop,
        Err(Error::Cancelled) => return Err(Error::Cancelled),
        Err(e) => {
            tracing::warn!("black bar detection failed: {e}");
            None
        }
    };
    let Some(stream) = media
        .video
        .iter_mut()
        .find(|v| v.type_index == vp.source_type_index && !v.attached_pic)
    else {
        return Ok(None);
    };
    stream.crop = crop;
    if crop.is_none() {
        return Ok(None);
    }
    let cropped = plan::build_plan(media, &ctx.profile, &ctx.caps.plan_capabilities());
    if cropped.is_blocked() || cropped == *plan {
        return Ok(None);
    }
    tracing::info!("black bars detected: {:?}", crop);
    Ok(Some(cropped))
}

async fn run_inner<F>(
    ctx: &RunnerContext,
    job: &Job,
    handle: &ProcessHandle,
    emit: &F,
    work_dir: &Path,
) -> Result<RunOutcome>
where
    F: Fn(RunnerEvent) + Send + Sync,
{
    emit(RunnerEvent::State(JobState::Preparing));
    let log_file = ctx.job_log_dir.join(format!("job-{}.log", job.id));
    emit(RunnerEvent::LogFile(log_file.clone()));

    // 1. Source still there and unchanged?
    let facts = file_facts(&job.media).map_err(|e| Error::io(&job.media.absolute_path, e))?;
    let (mut media, mut plan) = if facts.size_bytes != job.media.size_bytes
        || facts.modified_unix != job.media.modified_unix
    {
        tracing::info!(
            "source changed since scan, re-analysing: {}",
            facts.absolute_path.display()
        );
        let media = probe::analyze_file(&ctx.tools.ffprobe, &facts).await?;
        let plan = plan::build_plan(&media, &ctx.profile, &ctx.caps.plan_capabilities());
        emit(RunnerEvent::Replanned(Box::new(plan.clone())));
        (media, plan)
    } else {
        (job.media.clone(), job.plan.clone())
    };
    if plan.is_blocked() {
        let reasons: Vec<String> = plan.blockers.iter().map(|b| b.describe()).collect();
        return Err(Error::Plan(reasons.join("; ")));
    }

    // 1b. Damaged source (zero-filled after a broken copy)? Encoding would
    //     take hours and fail the duration check at the end.
    check_source_intact(&media).await?;

    // 2. Target conflict (re-checked right before work starts).
    if job.target.exists() && !job.replace_existing {
        let mut o = RunOutcome::finished(JobState::Skipped);
        o.error = Some(trf!("Target already exists: {}", job.target.display()));
        return Ok(o);
    }

    if work_dir.exists() {
        remove_work_dir(&ctx.work_root, work_dir);
    }
    tokio::fs::create_dir_all(work_dir)
        .await
        .map_err(|e| Error::io(work_dir, e))?;

    // Static HDR10 metadata for encodes that keep HDR (also needed when the
    // automatic method switches to HEVC later).
    if ctx.profile.hdr.strategy == HdrStrategy::Preserve && !plan.video_is_copied() {
        read_hdr_metadata(ctx, &mut media, &plan).await;
    }

    // Black bars: measured only for files that are actually re-encoded.
    if let Some(cropped) = detect_black_bars(ctx, &mut media, &plan, handle).await? {
        plan = cropped;
        emit(RunnerEvent::Replanned(Box::new(plan.clone())));
    }

    let encode_codec =
        |plan: &EncodePlan, profile: &EncodingProfile| match plan.video.as_ref().map(|v| &v.action)
        {
            Some(VideoAction::Encode { codec, .. }) => *codec,
            _ => profile.video.codec,
        };
    let encoder_label = |s: &EncoderSetup, plan: &EncodePlan, codec: TargetVideoCodec| {
        if plan.video_is_copied() {
            "copy".to_string()
        } else {
            s.encoder_label(codec)
        }
    };
    let mut profile = ctx.profile.clone();
    let mut codec = encode_codec(&plan, &profile);
    let mut setup = resolve_setup(ctx, &plan, codec);
    emit(RunnerEvent::Encoder {
        label: encoder_label(&setup, &plan, codec),
        fallback: false,
    });

    // 3. Smart CRF search / automatic method selection.
    let mut crf_override = None;
    let mut search_result: Option<QualityResult> = None;
    let mut fallback: Option<method::Fallback> = None;
    if quality::applies(&plan, &profile) && profile.quality.smart_crf {
        emit(RunnerEvent::State(JobState::QualitySearch));
        let selection = method::search(
            method::SearchInput {
                ctx,
                media: &media,
                plan: &plan,
                work_dir,
                log_file: log_file.clone(),
                handle,
            },
            emit,
        )
        .await?;
        if selection.plan != plan {
            plan = selection.plan;
            emit(RunnerEvent::Replanned(Box::new(plan.clone())));
        }
        profile = selection.profile;
        crf_override = selection.crf;
        search_result = Some(selection.quality);
        fallback = selection.fallback;
        codec = encode_codec(&plan, &profile);
        setup = resolve_setup(ctx, &plan, codec);
        emit(RunnerEvent::Encoder {
            label: encoder_label(&setup, &plan, codec),
            fallback: false,
        });
    }

    // 4. Main encode, with CPU fallback for hardware failures and H.264
    //    fallback when an automatically chosen HEVC encode fails.
    emit(RunnerEvent::State(JobState::Encoding));
    let temp_output = work_dir.join("output.mp4");
    let duration = media.duration_s();
    loop {
        let args = command::build_encode_args(&EncodeCommand {
            media: &media,
            plan: &plan,
            profile: &profile,
            setup: &setup,
            output: &temp_output,
            crf: crf_override,
            window: None,
        });
        let opts = RunOptions {
            ffmpeg: ctx.tools.ffmpeg.clone(),
            args,
            log_file: Some(log_file.clone()),
            nice: ctx.resources.nice_level,
            context: tr!("encoding").to_string(),
        };
        let result = run_ffmpeg(opts, handle, |u| {
            emit(RunnerEvent::Progress(JobProgress {
                fraction: u.fraction(duration),
                speed: u.speed,
                fps: u.fps,
                eta_s: u.eta_s(duration),
                out_time_s: u.out_time_s,
                current_size: u.total_size,
                probe_crf: None,
            }));
        })
        .await;
        match result {
            Ok(_) => break,
            Err(Error::ToolFailed { .. })
                if (setup.hw_encode.is_some() || setup.hw_decode.is_some())
                    && ctx.resources.fallback_to_cpu =>
            {
                tracing::warn!(
                    "hardware encode failed for job {}, falling back to CPU",
                    job.id
                );
                let _ = tokio::fs::remove_file(&temp_output).await;
                setup = EncoderSetup {
                    hw_encode: None,
                    hw_decode: None,
                    ..setup
                };
                emit(RunnerEvent::Encoder {
                    label: encoder_label(&setup, &plan, codec),
                    fallback: true,
                });
            }
            Err(e @ Error::ToolFailed { .. }) if fallback.is_some() => {
                let Some(fb) = fallback.take() else {
                    return Err(e);
                };
                tracing::warn!(
                    "HEVC encode failed for job {}, falling back to H.264: {e}",
                    job.id
                );
                let _ = tokio::fs::remove_file(&temp_output).await;
                let auto = search_result.as_ref().and_then(|q| q.auto.clone());
                let mut quality = fb.quality;
                quality.auto = auto.map(|mut a| {
                    a.choice = AutoChoice::Encode(TargetVideoCodec::H264);
                    a.notes.push(trf!("HEVC encode failed, H.264 used: {}", e));
                    a
                });
                search_result = Some(quality);
                plan = fb.plan;
                profile = fb.profile;
                crf_override = Some(fb.crf);
                codec = encode_codec(&plan, &profile);
                setup = resolve_setup(ctx, &plan, codec);
                emit(RunnerEvent::Replanned(Box::new(plan.clone())));
                emit(RunnerEvent::Encoder {
                    label: encoder_label(&setup, &plan, codec),
                    fallback: true,
                });
            }
            Err(e) => return Err(e),
        }
    }

    // 5. Sidecar subtitles.
    let mut sidecar_temp: Vec<(PathBuf, PathBuf)> = Vec::new(); // (temp, final)
    if plan.sidecars().next().is_some() {
        emit(RunnerEvent::State(JobState::Sidecars));
        let mut empty_tracks = Vec::new();
        for (i, sub) in plan.sidecars().enumerate() {
            let Some(final_path) = command::sidecar_path(&job.target, sub) else {
                continue;
            };
            let ext = final_path
                .extension()
                .map(|e| e.to_string_lossy().into_owned())
                .unwrap_or_default();
            let temp = work_dir.join(format!("sidecar-{i}.{ext}"));
            let args = command::build_sidecar_args(&media.absolute_path, sub, &temp);
            run_ffmpeg(
                RunOptions {
                    ffmpeg: ctx.tools.ffmpeg.clone(),
                    args,
                    log_file: Some(log_file.clone()),
                    nice: ctx.resources.nice_level,
                    context: tr!("subtitle export").to_string(),
                },
                handle,
                |_| {},
            )
            .await?;
            // empty = no file, 0 bytes, unreadable or no packets (an empty .mks
            // still has a container header, so the size alone is not enough)
            let empty = std::fs::metadata(&temp).map_or(true, |m| m.len() == 0)
                || probe::count_subtitle_packets(&ctx.tools.ffprobe, &temp, 0)
                    .await
                    .map_or(true, |n| n == 0);
            if empty {
                // An empty export is only acceptable if the source track
                // itself has no events; otherwise validation reports the loss.
                let packets = probe::count_subtitle_packets(
                    &ctx.tools.ffprobe,
                    &media.absolute_path,
                    sub.source_type_index,
                )
                .await
                .unwrap_or(u64::MAX);
                if packets == 0 {
                    let _ = std::fs::remove_file(&temp);
                    empty_tracks.push(sub.source_type_index + 1);
                    continue;
                }
            }
            sidecar_temp.push((temp, final_path));
        }
        if !empty_tracks.is_empty() {
            plan.warnings.extend(
                empty_tracks
                    .into_iter()
                    .map(|track| PlanWarning::SubtitleStreamEmpty { track }),
            );
            emit(RunnerEvent::Replanned(Box::new(plan.clone())));
        }
    }

    // 6. Validation.
    emit(RunnerEvent::State(JobState::Validating));
    emit(RunnerEvent::Progress(JobProgress::default()));
    let temps: Vec<PathBuf> = sidecar_temp.iter().map(|(t, _)| t.clone()).collect();
    let mut report = validate::validate(
        ValidateRequest {
            ffmpeg: &ctx.tools.ffmpeg,
            ffprobe: &ctx.tools.ffprobe,
            output: &temp_output,
            plan: &plan,
            source: &media,
            sidecars: &temps,
            decode_test: ctx.encoding.full_decode_test,
            log_file: Some(log_file.clone()),
            nice: ctx.resources.nice_level,
        },
        handle,
    )
    .await;
    if handle.is_cancelled() {
        return Err(Error::Cancelled);
    }

    // 7. Quality verification of the real output.
    let mut quality_result = search_result;
    let wants_check = quality::applies(&plan, &profile)
        && (profile.quality.metric != QualityMetric::Off || profile.quality_check_required());
    // The final check measures the same windows the search used.
    let verify_windows: Option<Vec<Window>> = quality_result
        .as_ref()
        .and_then(|q| q.auto.as_ref())
        .filter(|a| a.scene_windows)
        .map(|a| {
            a.window_starts
                .iter()
                .map(|s| Window {
                    start_s: *s,
                    duration_s: f64::from(profile.quality.sample_seconds.max(2)),
                })
                .collect()
        });
    if report.passed() && wants_check {
        let qctx = QualityContext {
            ffmpeg: &ctx.tools.ffmpeg,
            media: &media,
            plan: &plan,
            profile: &profile,
            setup: &setup,
            work_dir,
            log_file: Some(log_file.clone()),
            nice: ctx.resources.nice_level,
            has_vmaf: ctx.caps.has_vmaf(),
            windows: verify_windows.as_deref(),
        };
        let crf = crf_override.unwrap_or(profile.video.crf);
        match qctx.verify_output(&temp_output, crf, handle).await {
            Ok(mut q) => {
                // Keep the Smart CRF search trace next to the final measurement.
                if let Some(prev) = &quality_result {
                    q.search = prev.search.clone();
                    q.sample_bytes = prev.sample_bytes;
                    q.auto = prev.auto.clone();
                }
                let mandatory = profile.quality_check_required();
                report.checks.push(ValidationCheck {
                    kind: CheckKind::Quality,
                    passed: q.passed || !mandatory,
                    expected: format!("{} >= {}", q.metric.label(), q.threshold),
                    actual: format!("{:.4} (min {:.4})", q.score, q.min_score),
                });
                quality_result = Some(q);
            }
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(e) => report.checks.push(ValidationCheck {
                kind: CheckKind::Quality,
                passed: !profile.quality_check_required(),
                expected: tr!("measurement").to_string(),
                actual: e.to_string(),
            }),
        }
    }

    if !report.passed() {
        if ctx.encoding.quarantine_failed && temp_output.exists() {
            let qdir = ctx.work_root.join("quarantine");
            let name = format!(
                "job-{}-{}",
                job.id,
                job.target
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            );
            let _ = std::fs::create_dir_all(&qdir);
            if let Err(e) = std::fs::rename(&temp_output, qdir.join(&name)) {
                tracing::warn!("quarantine failed: {e}");
            }
        }
        return Ok(RunOutcome {
            state: Some(JobState::Failed),
            error: Some(trf!("Validation failed: {}", report.failure_summary())),
            validation: Some(report),
            quality: quality_result,
            ..RunOutcome::default()
        });
    }

    // 8. Finalize: publish main output and sidecars as one transaction
    //    (no clobber unless confirmed; all or nothing).
    emit(RunnerEvent::State(JobState::Finalizing));
    let result_size = tokio::fs::metadata(&temp_output)
        .await
        .ok()
        .map(|m| m.len());
    let replace = job.replace_existing;
    let mut files = vec![PlannedFile {
        role: FileRole::Main,
        source: temp_output.clone(),
        target: job.target.clone(),
        replace,
    }];
    for (temp, final_path) in sidecar_temp {
        let taken = |p: &Path| files.iter().any(|f| f.target == p);
        let free = |p: &Path| !taken(p) && (replace || !p.exists());
        let target = if free(&final_path) {
            final_path.clone()
        } else {
            match (1..1000)
                .map(|n| numbered_variant(&final_path, n))
                .find(|p| !taken(p) && !p.exists())
            {
                Some(p) => p,
                None => {
                    tracing::warn!("sidecar {} not placed: no free name", final_path.display());
                    continue;
                }
            }
        };
        files.push(PlannedFile {
            role: FileRole::Sidecar,
            source: temp,
            target,
            replace,
        });
    }
    let job_result = JobResult {
        result_size,
        quality: quality_result.clone(),
        validation: Some(report.clone()),
    };
    let journal_path = finalize::journal_path(&ctx.work_root, job.id);
    let job_id = job.id;
    let journal = tokio::task::spawn_blocking(move || {
        finalize::finalize(&RealFs, &journal_path, job_id, files, job_result)
    })
    .await
    .map_err(|e| Error::io(&job.target, io::Error::other(e)))?
    .map_err(|e| {
        if e.is_conflict() {
            Error::InvalidPaths(trf!("Target already exists: {}", e.path.display()))
        } else if e.rolled_back {
            Error::io(&e.path, e.error)
        } else {
            let msg = trf!(
                "{} – finalizing will be completed or undone at the next start",
                e.error
            );
            Error::io(&e.path, io::Error::new(e.error.kind(), msg))
        }
    })?;

    Ok(RunOutcome {
        state: Some(JobState::Done),
        error: None,
        result_size,
        quality: quality_result,
        validation: Some(report),
        sidecars: journal.sidecars(),
        target: journal.target().map(Path::to_path_buf),
        journal: Some(finalize::journal_path(&ctx.work_root, job.id)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_dir_cleanup_is_confined() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("work");
        let outside = dir.path().join("precious");
        std::fs::create_dir_all(&outside).unwrap();
        remove_work_dir(&root, &outside);
        assert!(outside.exists());
        std::fs::create_dir_all(job_work_dir(&root, 7)).unwrap();
        std::fs::create_dir_all(job_work_dir(&root, 8)).unwrap();
        std::fs::create_dir_all(root.join("quarantine")).unwrap();
        cleanup_orphans(&root, &[8]);
        assert!(!job_work_dir(&root, 7).exists());
        assert!(job_work_dir(&root, 8).exists());
        assert!(root.join("quarantine").exists());
    }

    #[test]
    fn pending_finalization_keeps_work_dir_and_blocks_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("work");
        std::fs::create_dir_all(job_work_dir(&root, 5)).unwrap();
        std::fs::create_dir_all(job_work_dir(&root, 6)).unwrap();
        let journal = finalize::journal_path(&root, 5);
        std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
        std::fs::write(&journal, b"{}").unwrap();
        cleanup_orphans(&root, &[]);
        assert!(job_work_dir(&root, 5).exists(), "recovery may need it");
        assert!(!job_work_dir(&root, 6).exists());
        assert!(journal.exists(), "journal is not a work dir");
    }
}
