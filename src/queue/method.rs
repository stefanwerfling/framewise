//! Smart CRF search and, for profiles that ask for it, automatic method
//! selection (profile E):
//!
//! 1. Sample windows from the hardest and some typical scenes
//!    ([`scenes`]), instead of three evenly spaced ones.
//! 2. CRF search for H.264 – the default and always the fallback.
//! 3. If allowed and within the time budget: CRF search for HEVC. HEVC is
//!    only chosen when it meets the quality floor *and* is clearly smaller.
//! 4. If even the best encode saves too little, the video stream is copied
//!    (when the source stream can be copied at all).
//!
//! HDR source with "Preserve HDR" and HEVC allowed: H.264 cannot keep HDR,
//! so HEVC is the only candidate (step 2 searches HEVC, step 3 is skipped).
//!
//! The quality floor always wins over file size.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::job::JobProgress;
use super::runner::{RunnerContext, RunnerEvent, resolve_setup};
use crate::core::error::{Error, Result};
use crate::encoding::command::Window;
use crate::encoding::plan::{self, EncodePlan, VideoAction};
use crate::encoding::process::ProcessHandle;
use crate::encoding::profile::{EncodingProfile, HdrStrategy, TargetVideoCodec};
use crate::encoding::quality::{
    AutoChoice, AutoDecision, MethodCandidate, QualityContext, QualityResult, sample_windows,
    snap_to_frame_gap,
};
use crate::encoding::scenes::{self, Complexity};
use crate::media::model::MediaFile;
use crate::trf;

/// Outcome of the search: what the main encode should do.
pub struct Selection {
    pub profile: EncodingProfile,
    pub plan: EncodePlan,
    /// CRF for the main encode (`None` when the video is copied).
    pub crf: Option<u8>,
    pub quality: QualityResult,
    /// H.264 settings to fall back to if the chosen HEVC encode fails.
    pub fallback: Option<Fallback>,
}

pub struct Fallback {
    pub profile: EncodingProfile,
    pub plan: EncodePlan,
    pub crf: u8,
    pub quality: QualityResult,
}

/// Below this remaining budget an HEVC search is not even started.
const MIN_HEVC_BUDGET: Duration = Duration::from_secs(120);

pub struct SearchInput<'a> {
    pub ctx: &'a RunnerContext,
    pub media: &'a MediaFile,
    pub plan: &'a EncodePlan,
    pub work_dir: &'a Path,
    pub log_file: PathBuf,
    pub handle: &'a ProcessHandle,
}

pub async fn search<F>(input: SearchInput<'_>, emit: &F) -> Result<Selection>
where
    F: Fn(RunnerEvent) + Send + Sync,
{
    let started = Instant::now();
    let ctx = input.ctx;
    let profile = &ctx.profile;
    let q = &profile.quality;
    let budget = Duration::from_secs(u64::from(q.analysis_budget_min) * 60);
    let mut notes = Vec::new();

    // 1. Sample windows.
    let duration = input.media.duration_s();
    let complexity = match (q.scene_windows, duration, input.plan.video.as_ref()) {
        (true, Some(_), Some(v)) => {
            match scenes::measure(
                &ctx.tools.ffprobe,
                &input.media.absolute_path,
                v.source_type_index,
                input.handle,
            )
            .await
            {
                Ok(c) if c.is_usable() => Some(c),
                Ok(_) => None,
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(e) => {
                    tracing::warn!("scene complexity not available: {e}");
                    None
                }
            }
        }
        _ => None,
    };
    let fps = input.media.primary_video().and_then(|v| v.fps());
    let windows: Option<Vec<Window>> = complexity
        .as_ref()
        .zip(duration)
        .map(|(c, d)| c.pick_windows(q.sample_count, q.sample_seconds, d))
        .filter(|w| !w.is_empty())
        .map(|w| w.into_iter().map(|w| snap_to_frame_gap(w, fps)).collect());
    if q.scene_windows && windows.is_none() {
        notes.push(trf!("Scene analysis not possible, samples evenly spaced"));
    }

    let run = |profile: EncodingProfile, plan: EncodePlan| {
        let windows = windows.clone();
        let log_file = input.log_file.clone();
        async move {
            let t = Instant::now();
            let codec = match plan.video.as_ref().map(|v| &v.action) {
                Some(VideoAction::Encode { codec, .. }) => *codec,
                _ => profile.video.codec,
            };
            let setup = resolve_setup(ctx, &plan, codec);
            let qctx = QualityContext {
                ffmpeg: &ctx.tools.ffmpeg,
                media: input.media,
                plan: &plan,
                profile: &profile,
                setup: &setup,
                work_dir: input.work_dir,
                log_file: Some(log_file),
                nice: ctx.resources.nice_level,
                has_vmaf: ctx.caps.has_vmaf(),
                windows: windows.as_deref(),
            };
            let result = qctx
                .find_crf(input.handle, |crf| {
                    emit(RunnerEvent::Progress(JobProgress {
                        probe_crf: Some(crf),
                        ..JobProgress::default()
                    }));
                })
                .await?;
            Ok::<_, Error>(Candidate {
                result,
                secs: t.elapsed().as_secs(),
                profile,
                plan,
                codec,
            })
        }
    };

    // HEVC variant of the profile, starting `+2` above an H.264 CRF.
    let hevc_profile = |h264_crf: u8| {
        let mut hevc = profile.clone();
        hevc.video.codec = TargetVideoCodec::Hevc;
        hevc.video.preset = q.hevc_preset.clone();
        hevc.video.allow_copy = false;
        // x265 reaches a given quality at a higher CRF than x264.
        hevc.video.crf = h264_crf.saturating_add(2);
        hevc.quality.crf_max = q.crf_max.saturating_add(4);
        hevc
    };

    // HDR to keep, but the profile codec cannot: search HEVC only.
    let keep_hdr = if q.compare_hevc
        && profile.video.codec == TargetVideoCodec::H264
        && profile.hdr.strategy == HdrStrategy::Preserve
    {
        let hevc = hevc_profile(profile.video.crf);
        let hevc_plan = plan::build_plan(input.media, &hevc, &ctx.caps.plan_capabilities());
        let keeps = matches!(
            hevc_plan.video.as_ref().map(|v| &v.action),
            Some(VideoAction::Encode { hdr, .. }) if hdr.is_preserve()
        );
        (keeps && !hevc_plan.is_blocked()).then_some((hevc, hevc_plan))
    } else {
        None
    };
    let keeping_hdr = keep_hdr.is_some();

    // 2. Profile codec (H.264 by default), or HEVC when HDR is kept.
    let a = match keep_hdr {
        Some((hevc, hevc_plan)) => {
            notes.push(trf!(
                "HDR is kept: only HEVC searched (H.264 cannot keep HDR)"
            ));
            run(hevc, hevc_plan).await?
        }
        None => run(profile.clone(), input.plan.clone()).await?,
    };
    let effective_windows = windows.clone().unwrap_or_else(|| {
        sample_windows(duration, q.sample_count, q.sample_seconds)
            .into_iter()
            .map(|w| snap_to_frame_gap(w, fps))
            .collect()
    });
    let source_bytes = source_bytes(input.media, complexity.as_ref(), &effective_windows);
    let ratio = |c: &Candidate| size_ratio(&c.result, source_bytes);

    // 3. HEVC candidate.
    let mut b: Option<Candidate> = None;
    if q.compare_hevc && a.codec == TargetVideoCodec::H264 {
        let remaining = budget.saturating_sub(started.elapsed());
        let hevc = hevc_profile(a.result.crf);
        let hevc_plan = plan::build_plan(input.media, &hevc, &ctx.caps.plan_capabilities());
        if !ctx.caps.plan_capabilities().libx265 {
            notes.push(trf!("HEVC not compared: libx265 is not available"));
        } else if remaining < MIN_HEVC_BUDGET {
            notes.push(trf!(
                "HEVC not compared: time budget of {} min used up",
                q.analysis_budget_min
            ));
        } else if hevc_plan.is_blocked()
            || !same_picture(input.plan, &hevc_plan)
            || !matches!(
                hevc_plan.video.as_ref().map(|v| &v.action),
                Some(VideoAction::Encode { .. })
            )
        {
            notes.push(trf!(
                "HEVC not compared: the output would not be comparable"
            ));
        } else {
            // The budget is a hard limit: a search that does not finish in
            // time is stopped (dropping it kills ffmpeg) and H.264 is used.
            // Leftover sample files go with the job's work directory.
            match tokio::time::timeout(remaining, run(hevc, hevc_plan)).await {
                Ok(result) => b = Some(result?),
                Err(_) => notes.push(trf!(
                    "HEVC comparison stopped: time budget of {} min reached",
                    q.analysis_budget_min
                )),
            }
        }
    }

    let hevc_wins = b.as_ref().is_some_and(|b| {
        b.result.passed
            && (!a.result.passed
                || matches!(
                    (ratio(b), ratio(&a)),
                    (Some(rb), Some(ra)) if rb <= ra * (1.0 - q.hevc_min_saving)
                ))
    });

    let mut candidates = vec![candidate_info(&a, ratio(&a))];
    if let Some(b) = &b {
        candidates.push(candidate_info(b, ratio(b)));
    }
    let window_starts: Vec<f64> = effective_windows.iter().map(|w| w.start_s).collect();
    let scene_windows = windows.is_some();
    let decision = |choice, notes: Vec<String>| AutoDecision {
        choice,
        candidates: candidates.clone(),
        window_starts: window_starts.clone(),
        scene_windows,
        notes,
    };

    let (chosen, fallback) = match b {
        Some(b) if hevc_wins => {
            let fb = Fallback {
                profile: a.profile,
                plan: a.plan,
                crf: a.result.crf,
                quality: a.result,
            };
            (b, Some(fb))
        }
        _ => (a, None),
    };

    // 4. Not worth re-encoding? Copy the video stream if possible.
    let chosen_ratio = ratio(&chosen);
    if q.min_saving > 0.0
        && let Some(r) = chosen_ratio
        && r > 1.0 - q.min_saving
    {
        // When HDR is kept, the copy must keep it too (HEVC target).
        let mut copy = if keeping_hdr {
            chosen.profile.clone()
        } else {
            profile.clone()
        };
        copy.video.allow_copy = true;
        copy.video.copy_max_bpp = f64::MAX;
        let copy_plan = plan::build_plan(input.media, &copy, &ctx.caps.plan_capabilities());
        if copy_plan.video_is_copied() && !copy_plan.is_blocked() {
            let mut quality = chosen.result;
            quality.auto = Some(decision(AutoChoice::CopyVideo, notes));
            return Ok(Selection {
                profile: copy,
                plan: copy_plan,
                crf: None,
                quality,
                fallback: None,
            });
        }
        notes.push(trf!(
            "Saving below {} %, but the video stream cannot be copied",
            (q.min_saving * 100.0).round()
        ));
    }

    let mut quality = chosen.result;
    if q.compare_hevc || q.scene_windows || q.min_saving > 0.0 {
        quality.auto = Some(decision(AutoChoice::Encode(chosen.codec), notes));
    }
    Ok(Selection {
        crf: Some(quality.crf),
        profile: chosen.profile,
        plan: chosen.plan,
        quality,
        fallback,
    })
}

struct Candidate {
    result: QualityResult,
    secs: u64,
    profile: EncodingProfile,
    plan: EncodePlan,
    codec: TargetVideoCodec,
}

fn candidate_info(c: &Candidate, ratio: Option<f64>) -> MethodCandidate {
    MethodCandidate {
        codec: c.codec,
        preset: c.profile.video.preset.clone(),
        crf: c.result.crf,
        score: c.result.score,
        min_score: c.result.min_score,
        passed: c.result.passed,
        size_ratio: ratio,
        search_secs: c.secs,
    }
}

/// Source video bytes inside the sample windows: from the packet map when
/// available, otherwise from the average video bitrate.
fn source_bytes(
    media: &MediaFile,
    complexity: Option<&Complexity>,
    windows: &[Window],
) -> Option<u64> {
    match complexity {
        Some(c) => Some(windows.iter().map(|w| c.bytes_in(*w)).sum()),
        None => {
            let seconds: f64 = windows.iter().map(|w| w.duration_s).sum();
            media
                .video_bitrate()
                .map(|br| (br as f64 / 8.0 * seconds) as u64)
        }
    }
    .filter(|b| *b > 0)
}

/// Estimated result size relative to the source (video only).
fn size_ratio(result: &QualityResult, source_bytes: Option<u64>) -> Option<f64> {
    let (sample, source) = (result.sample_bytes?, source_bytes?);
    (source > 0).then(|| sample as f64 / source as f64)
}

/// Both plans produce the same picture (resolution, HDR handling,
/// deinterlacing), so their quality scores are comparable.
fn same_picture(a: &EncodePlan, b: &EncodePlan) -> bool {
    match (
        a.video.as_ref().map(|v| &v.action),
        b.video.as_ref().map(|v| &v.action),
    ) {
        (
            Some(VideoAction::Encode {
                resolution: ra,
                hdr: ha,
                deinterlace: da,
                ..
            }),
            Some(VideoAction::Encode {
                resolution: rb,
                hdr: hb,
                deinterlace: db,
                ..
            }),
        ) => ra == rb && ha == hb && da == db,
        _ => false,
    }
}
