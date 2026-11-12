//! Perceptual quality measurement (SSIM / VMAF) and CRF search.
//!
//! Approach (per-title light): encode a few short samples at candidate CRF
//! values, compare each against the identically pre-processed source and pick
//! the **highest CRF that still meets the minimum quality**. A smaller file is
//! never considered "better" by itself – the quality floor always wins.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::command::{self, EncodeCommand, EncoderSetup, Window};
use super::plan::{EncodePlan, HdrConversion, VideoAction};
use super::process::{ProcessHandle, RunOptions, run_ffmpeg};
use super::profile::{EncodingProfile, QualityMetric, TargetVideoCodec};
use crate::core::error::{Error, Result};
use crate::media::model::MediaFile;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QualityResult {
    pub metric: QualityMetric,
    /// Average score over all samples.
    pub score: f64,
    /// Lowest sample score.
    pub min_score: f64,
    pub threshold: f64,
    pub crf: u8,
    pub samples: u32,
    pub passed: bool,
    /// CRF search trace `(crf, score)`.
    pub search: Vec<(u8, f64)>,
    /// VMAF was requested but is not available in this ffmpeg build.
    pub vmaf_unavailable: bool,
    /// Video bytes of all samples at the chosen CRF (search only).
    #[serde(default)]
    pub sample_bytes: Option<u64>,
    /// The minimum applies to the weakest sample (`min_score`).
    #[serde(default)]
    pub judge_min: bool,
    /// How the automatic method selection decided (profile E).
    #[serde(default)]
    pub auto: Option<AutoDecision>,
}

impl QualityResult {
    /// The value compared against the threshold.
    pub fn judged(&self) -> f64 {
        if self.judge_min {
            self.min_score
        } else {
            self.score
        }
    }
}

/// One method tried by the automatic selection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MethodCandidate {
    pub codec: TargetVideoCodec,
    pub preset: String,
    pub crf: u8,
    pub score: f64,
    pub min_score: f64,
    pub passed: bool,
    /// Estimated video size relative to the source video (0.4 = 40 %).
    pub size_ratio: Option<f64>,
    /// Wall time of this candidate's search.
    pub search_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AutoChoice {
    Encode(TargetVideoCodec),
    /// Re-encoding would save too little; the video stream is copied.
    CopyVideo,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutoDecision {
    pub choice: AutoChoice,
    pub candidates: Vec<MethodCandidate>,
    /// Start times of the sample windows (seconds).
    pub window_starts: Vec<f64>,
    /// Windows were chosen by source complexity.
    pub scene_windows: bool,
    /// Why a candidate was not tried (e.g. time budget).
    pub notes: Vec<String>,
}

/// Evenly distributed sample windows, avoiding intro and credits.
pub fn sample_windows(duration_s: Option<f64>, count: u32, seconds: u32) -> Vec<Window> {
    let len = f64::from(seconds.max(2));
    let Some(d) = duration_s.filter(|d| *d > 0.0) else {
        return vec![Window {
            start_s: 0.0,
            duration_s: len,
        }];
    };
    let count = count.max(1);
    if d <= len * f64::from(count) {
        return vec![Window {
            start_s: 0.0,
            duration_s: d.min(len),
        }];
    }
    (1..=count)
        .map(|i| {
            let center = d * f64::from(i) / f64::from(count + 1);
            Window {
                start_s: (center - len / 2.0).max(0.0),
                duration_s: len,
            }
        })
        .collect()
}

/// Moves a window start to the middle between two frames, so that seeking
/// the source (MKV, millisecond timestamps) and the result (MP4, exact
/// timestamps) lands on the same first frame despite rounding.
pub fn snap_to_frame_gap(w: Window, fps: Option<f64>) -> Window {
    match fps.filter(|f| f.is_finite() && *f > 1.0) {
        Some(fps) if w.start_s > 0.0 => Window {
            start_s: ((w.start_s * fps).floor() + 0.5) / fps,
            duration_s: w.duration_s,
        },
        _ => w,
    }
}

/// `SSIM Y:0.99 U:0.99 V:0.99 All:0.987654 (19.08)` → 0.987654
pub fn parse_ssim(stderr: &str) -> Option<f64> {
    stderr
        .lines()
        .rev()
        .find(|l| l.contains("SSIM") && l.contains("All:"))
        .and_then(|l| l.split("All:").nth(1))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|v| v.parse().ok())
}

/// `VMAF score: 95.123456` (libvmaf v2) or `VMAF score = 95.1` (v1).
pub fn parse_vmaf(stderr: &str) -> Option<f64> {
    stderr
        .lines()
        .rev()
        .find(|l| l.contains("VMAF score"))
        .and_then(|l| l.rsplit([':', '=']).next())
        .and_then(|v| v.trim().parse().ok())
}

/// Reference pre-processing: the same deinterlace/scale/tonemap as the encode,
/// so the metric measures encoding loss only.
fn reference_filters(plan: &EncodePlan, profile: &EncodingProfile, setup: &EncoderSetup) -> String {
    let ten_bit = command::encodes_10bit(plan, setup);
    let format = || format!("format={}", command::pix_fmt(ten_bit));
    match plan.video.as_ref().map(|v| &v.action) {
        Some(VideoAction::Encode {
            resolution,
            hdr,
            deinterlace,
            ..
        }) => command::video_filters(resolution, hdr, *deinterlace, profile, None, ten_bit)
            .unwrap_or_else(format),
        _ => format(),
    }
}

pub struct QualityContext<'a> {
    pub ffmpeg: &'a Path,
    pub media: &'a MediaFile,
    pub plan: &'a EncodePlan,
    pub profile: &'a EncodingProfile,
    pub setup: &'a EncoderSetup,
    pub work_dir: &'a Path,
    pub log_file: Option<PathBuf>,
    pub nice: i32,
    pub has_vmaf: bool,
    /// Sample windows chosen by the caller (e.g. by scene complexity);
    /// `None` = evenly spaced from the profile settings.
    pub windows: Option<&'a [Window]>,
}

/// Result of encoding and measuring all samples at one CRF.
#[derive(Debug, Clone, Copy)]
struct Measure {
    avg: f64,
    min: f64,
    bytes: u64,
}

impl Measure {
    fn judged(&self, by_min: bool) -> f64 {
        if by_min { self.min } else { self.avg }
    }
}

impl QualityContext<'_> {
    fn metric(&self) -> (QualityMetric, f64, bool) {
        let q = &self.profile.quality;
        match q.metric {
            QualityMetric::Vmaf if self.has_vmaf => (QualityMetric::Vmaf, q.min_vmaf, false),
            QualityMetric::Vmaf => (QualityMetric::Ssim, q.min_ssim, true),
            QualityMetric::Off | QualityMetric::Ssim => (QualityMetric::Ssim, q.min_ssim, false),
        }
    }

    fn windows(&self) -> Vec<Window> {
        if let Some(w) = self.windows {
            return w.to_vec();
        }
        let q = &self.profile.quality;
        let fps = self.media.primary_video().and_then(|v| v.fps());
        sample_windows(self.media.duration_s(), q.sample_count, q.sample_seconds)
            .into_iter()
            .map(|w| snap_to_frame_gap(w, fps))
            .collect()
    }

    /// Pixel format both inputs are compared in: the encoder's output format
    /// (the reference filters end in the same format).
    fn measure_format(&self) -> &'static str {
        command::pix_fmt(command::encodes_10bit(self.plan, self.setup))
    }

    /// Source/reference input and the filters that turn it into the
    /// encoder's view: (path, seek window, video stream index, filters).
    fn reference_input<'r>(
        &'r self,
        reference: Reference<'r>,
    ) -> (&'r Path, Option<Window>, u32, String) {
        match reference {
            Reference::Source(window) => (
                self.media.absolute_path.as_path(),
                Some(window),
                self.plan.video.as_ref().map_or(0, |v| v.source_type_index),
                reference_filters(self.plan, self.profile, self.setup),
            ),
            // Already filtered to the encoder's input format.
            Reference::Prefiltered(path) => (path, None, 0, "null".to_string()),
        }
    }

    async fn run_measurement(
        &self,
        input_args: Vec<OsString>,
        graph: String,
        outputs: &[&str],
        handle: &ProcessHandle,
    ) -> Result<String> {
        let mut args: Vec<OsString> = [
            "-hide_banner",
            "-nostdin",
            "-loglevel",
            "info",
            "-progress",
            "pipe:1",
            "-nostats",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        args.extend(input_args);
        args.extend(["-lavfi".into(), graph.into()]);
        for o in outputs {
            if !o.is_empty() {
                args.extend(["-map".into(), (*o).into()]);
            }
            args.extend(["-f".into(), "null".into(), "-".into()]);
        }
        let out = run_ffmpeg(
            RunOptions {
                ffmpeg: self.ffmpeg.to_path_buf(),
                args,
                log_file: self.log_file.clone(),
                nice: self.nice,
                context: "quality measurement".into(),
            },
            handle,
            |_| {},
        )
        .await?;
        Ok(out.stderr_tail)
    }

    /// Compare `distorted` (optionally windowed) against the reference.
    /// `offset` > 0 skips that many leading distorted frames, < 0 leading
    /// reference frames (see [`Self::frame_offset`]).
    async fn compare(
        &self,
        distorted: &Path,
        distorted_window: Option<Window>,
        reference: Reference<'_>,
        offset: i32,
        metric: QualityMetric,
        handle: &ProcessHandle,
    ) -> Result<f64> {
        let (ref_input, ref_window, ref_stream, ref_filters) = self.reference_input(reference);
        let fmt = self.measure_format();
        // libvmaf is single-threaded by default; follow the configured CPU
        // thread limit (0 = all cores) so the measurement stays throttled too
        let threads = if self.setup.threads > 0 {
            self.setup.threads
        } else {
            std::thread::available_parallelism().map_or(4, |n| n.get())
        };
        let compare = match metric {
            QualityMetric::Vmaf => format!("libvmaf=n_threads={threads}:shortest=1"),
            _ => "ssim=shortest=1".to_string(),
        };
        let skip = |n: i32| {
            if n > 0 {
                format!("trim=start_frame={n},")
            } else {
                String::new()
            }
        };
        let (dskip, rskip) = (skip(offset), skip(-offset));
        let graph = format!(
            "[0:v:0]{dskip}format={fmt},{PAIR_BY_NUMBER}[dist];[1:v:{ref_stream}]{ref_filters},{rskip}{PAIR_BY_NUMBER}[ref];[dist][ref]{compare}"
        );
        let inputs = input_args(distorted, distorted_window, ref_input, ref_window);
        let stderr = self.run_measurement(inputs, graph, &[""], handle).await?;
        let score = match metric {
            QualityMetric::Vmaf => parse_vmaf(&stderr),
            _ => parse_ssim(&stderr),
        };
        score.ok_or_else(|| Error::ToolFailed {
            tool: "ffmpeg",
            code: None,
            context: "quality measurement".into(),
            stderr: "no score in output".into(),
        })
    }

    /// Frame offset between result and source at a seek point, determined by
    /// content: SSIM of the first frames at offsets −1, 0, +1. Seeking a MP4
    /// result and a MKV source to the same time can land one frame apart
    /// (seen with ffmpeg 4.4); measuring misaligned frames would report a
    /// far too low quality.
    async fn frame_offset(
        &self,
        distorted: &Path,
        window: Window,
        handle: &ProcessHandle,
    ) -> Result<i32> {
        let (ref_input, ref_window, ref_stream, ref_filters) =
            self.reference_input(Reference::Source(window));
        let fmt = self.measure_format();
        let n = ALIGN_FRAMES + 1;
        let graph = format!(
            "[0:v:0]format={fmt},{PAIR_BY_NUMBER},trim=end_frame={n},split=3[d0][d1][d2];\
             [1:v:{ref_stream}]{ref_filters},{PAIR_BY_NUMBER},trim=end_frame={n},split=3[r0][r1][r2];\
             [r0]trim=start_frame=1,setpts=N[r0s];[d0][r0s]ssim=shortest=1[o0];\
             [d1][r1]ssim=shortest=1[o1];\
             [d2]trim=start_frame=1,setpts=N[d2s];[d2s][r2]ssim=shortest=1[o2]"
        );
        let inputs = input_args(distorted, Some(window), ref_input, ref_window);
        let stderr = self
            .run_measurement(inputs, graph, &["[o0]", "[o1]", "[o2]"], handle)
            .await?;
        let scores = parse_ssim_all(&stderr);
        if scores.len() != 3 {
            return Err(Error::ToolFailed {
                tool: "ffmpeg",
                code: None,
                context: "frame alignment".into(),
                stderr: format!("expected 3 SSIM values, got {}", scores.len()),
            });
        }
        Ok(pick_offset([scores[0], scores[1], scores[2]]))
    }

    /// Lossless pre-filtered reference per sample window (CPU encodes only).
    /// Returns `None` when the plan cannot use references; the search then
    /// reads the source directly as before.
    async fn prepare_references(
        &self,
        windows: &[Window],
        handle: &ProcessHandle,
    ) -> Result<Option<Vec<PathBuf>>> {
        let mut refs = Vec::new();
        for (i, window) in windows.iter().enumerate() {
            let path = self.work_dir.join(format!("reference_{i}.mkv"));
            let _ = tokio::fs::remove_file(&path).await;
            let cmd = self.encode_command(&path, None);
            let Some(args) = command::build_reference_args(&cmd, *window, &path) else {
                return Ok(None);
            };
            let run = run_ffmpeg(
                RunOptions {
                    ffmpeg: self.ffmpeg.to_path_buf(),
                    args,
                    log_file: self.log_file.clone(),
                    nice: self.nice,
                    context: "quality reference".into(),
                },
                handle,
                |_| {},
            )
            .await;
            refs.push(path);
            if let Err(e) = run {
                remove_all(&refs).await;
                return Err(e);
            }
        }
        Ok(Some(refs))
    }

    fn encode_command<'b>(&'b self, output: &'b Path, crf: Option<u8>) -> EncodeCommand<'b> {
        EncodeCommand {
            media: self.media,
            plan: self.plan,
            profile: self.profile,
            setup: self.setup,
            output,
            crf,
            window: None,
        }
    }

    /// Encode all sample windows at `crf` and measure them.
    async fn measure_crf(
        &self,
        crf: u8,
        metric: QualityMetric,
        refs: Option<&[PathBuf]>,
        handle: &ProcessHandle,
    ) -> Result<Measure> {
        let mut scores = Vec::new();
        let mut bytes = 0u64;
        for (i, window) in self.windows().into_iter().enumerate() {
            let sample = self.work_dir.join(format!("sample_{i}_crf{crf}.mkv"));
            let _ = tokio::fs::remove_file(&sample).await;
            let reference_file = refs.and_then(|r| r.get(i));
            let args = match reference_file {
                Some(r) => command::build_reference_sample_args(
                    &self.encode_command(&sample, Some(crf)),
                    r,
                ),
                None => None,
            }
            .unwrap_or_else(|| {
                command::build_encode_args(&EncodeCommand {
                    window: Some(window),
                    ..self.encode_command(&sample, Some(crf))
                })
            });
            let run = run_ffmpeg(
                RunOptions {
                    ffmpeg: self.ffmpeg.to_path_buf(),
                    args,
                    log_file: self.log_file.clone(),
                    nice: self.nice,
                    context: format!("sample encode CRF {crf}"),
                },
                handle,
                |_| {},
            )
            .await;
            if let Err(e) = run {
                let _ = tokio::fs::remove_file(&sample).await;
                return Err(e);
            }
            let reference = match reference_file {
                Some(r) => Reference::Prefiltered(r),
                None => Reference::Source(window),
            };
            bytes += tokio::fs::metadata(&sample).await.map_or(0, |m| m.len());
            let score = self
                .compare(&sample, None, reference, 0, metric, handle)
                .await;
            let _ = tokio::fs::remove_file(&sample).await;
            scores.push(score?);
        }
        let avg = scores.iter().sum::<f64>() / scores.len().max(1) as f64;
        let min = scores.iter().copied().fold(f64::INFINITY, f64::min);
        Ok(Measure { avg, min, bytes })
    }

    /// Search for the highest CRF meeting the threshold (see [`next_crf`]).
    pub async fn find_crf<F>(&self, handle: &ProcessHandle, on_step: F) -> Result<QualityResult>
    where
        F: FnMut(u8),
    {
        let windows = self.windows();
        let refs = self.prepare_references(&windows, handle).await?;
        let result = self.search(refs.as_deref(), handle, on_step).await;
        if let Some(r) = &refs {
            remove_all(r).await;
        }
        result
    }

    async fn search<F>(
        &self,
        refs: Option<&[PathBuf]>,
        handle: &ProcessHandle,
        mut on_step: F,
    ) -> Result<QualityResult>
    where
        F: FnMut(u8),
    {
        let (metric, threshold, vmaf_unavailable) = self.metric();
        let q = &self.profile.quality;
        let range = (q.crf_min.min(q.crf_max), q.crf_max.max(q.crf_min));
        let start = self.profile.video.crf;
        let slope = metric.typical_slope();
        let by_min = q.judge_min;
        let mut measured: Vec<(u8, Measure)> = Vec::new();
        loop {
            let points: Vec<(u8, f64)> = measured
                .iter()
                .map(|(c, m)| (*c, m.judged(by_min)))
                .collect();
            let Some(crf) = next_crf(range, start, threshold, &points, slope) else {
                break;
            };
            on_step(crf);
            let m = self.measure_crf(crf, metric, refs, handle).await?;
            measured.push((crf, m));
        }
        let search: Vec<(u8, f64)> = measured
            .iter()
            .map(|(c, m)| (*c, m.judged(by_min)))
            .collect();
        let best = measured
            .iter()
            .filter(|(_, m)| m.judged(by_min) >= threshold)
            .max_by_key(|(c, _)| *c)
            .copied();
        let (crf, m, passed) = match best {
            Some((crf, m)) => (crf, m, true),
            None => {
                // Even the best allowed CRF misses the target: use it anyway,
                // the final check decides (and fails the job if mandatory).
                let crf = range.0;
                let m = match measured.iter().find(|(c, _)| *c == crf) {
                    Some((_, m)) => *m,
                    None => self.measure_crf(crf, metric, refs, handle).await?,
                };
                (crf, m, false)
            }
        };
        let (score, min_score) = (m.avg, m.min);
        Ok(QualityResult {
            metric,
            score,
            min_score,
            threshold,
            crf,
            samples: self.windows().len() as u32,
            passed,
            search,
            vmaf_unavailable,
            sample_bytes: Some(m.bytes),
            judge_min: by_min,
            auto: None,
        })
    }

    /// Measure the finished output against the source on the sample windows.
    pub async fn verify_output(
        &self,
        output: &Path,
        crf: u8,
        handle: &ProcessHandle,
    ) -> Result<QualityResult> {
        let (metric, threshold, vmaf_unavailable) = self.metric();
        let mut scores = Vec::new();
        for window in self.windows() {
            scores.push(
                {
                    let offset = match self.frame_offset(output, window, handle).await {
                        Ok(o) => o,
                        Err(Error::Cancelled) => return Err(Error::Cancelled),
                        Err(e) => {
                            tracing::warn!("frame alignment failed, assuming none: {e}");
                            0
                        }
                    };
                    if offset != 0 {
                        tracing::info!(
                            "quality window {:.1}s: frame offset {offset}",
                            window.start_s
                        );
                    }
                    self.compare(
                        output,
                        Some(window),
                        Reference::Source(window),
                        offset,
                        metric,
                        handle,
                    )
                }
                .await?,
            );
        }
        let score = scores.iter().sum::<f64>() / scores.len().max(1) as f64;
        let min_score = scores.iter().copied().fold(f64::INFINITY, f64::min);
        Ok(QualityResult {
            metric,
            score,
            min_score,
            threshold,
            crf,
            samples: scores.len() as u32,
            passed: if self.profile.quality.judge_min {
                min_score
            } else {
                score
            } >= threshold,
            search: Vec::new(),
            vmaf_unavailable,
            sample_bytes: None,
            judge_min: self.profile.quality.judge_min,
            auto: None,
        })
    }
}

/// Whether a quality step is needed for this plan.
pub fn applies(plan: &EncodePlan, profile: &EncodingProfile) -> bool {
    let encodes = matches!(
        plan.video.as_ref().map(|v| &v.action),
        Some(VideoAction::Encode { .. })
    );
    encodes
        && (profile.quality.metric != QualityMetric::Off
            || profile.quality.smart_crf
            || profile.quality_check_required())
}

/// Tone mapping changes the image intentionally; metrics are computed against
/// the tone-mapped reference, which is what [`reference_filters`] produces.
pub fn reference_is_tonemapped(plan: &EncodePlan) -> bool {
    matches!(
        plan.video.as_ref().map(|v| &v.action),
        Some(VideoAction::Encode {
            hdr: HdrConversion::TonemapToSdr { .. },
            ..
        })
    )
}

/// Frames are paired by number, not by timestamp: MKV rounds timestamps to
/// milliseconds (0.042) while MP4 keeps them exact (0.041708), and timestamp
/// pairing then compares neighbouring frames – real Blu-ray samples measured
/// VMAF 29 instead of 98. A common time base keeps the pairing exact.
const PAIR_BY_NUMBER: &str = "settb=1/25,setpts=N";
/// Frames compared to detect a seek offset.
const ALIGN_FRAMES: u32 = 12;

fn input_args(
    distorted: &Path,
    distorted_window: Option<Window>,
    reference: &Path,
    reference_window: Option<Window>,
) -> Vec<OsString> {
    let mut args = Vec::new();
    for (path, window) in [(distorted, distorted_window), (reference, reference_window)] {
        if let Some(w) = window {
            args.extend([
                "-ss".into(),
                format!("{:.3}", w.start_s).into(),
                "-t".into(),
                format!("{:.3}", w.duration_s).into(),
            ]);
        }
        args.extend(["-i".into(), path.as_os_str().to_owned()]);
    }
    args
}

/// All SSIM results of a graph, ordered by filter instance
/// (`[Parsed_ssim_18 @ …] SSIM … All:0.98 …`).
fn parse_ssim_all(stderr: &str) -> Vec<f64> {
    let mut found: Vec<(u32, f64)> = stderr
        .lines()
        .filter(|l| l.contains("SSIM") && l.contains("All:"))
        .filter_map(|l| {
            let idx = l
                .split("Parsed_ssim_")
                .nth(1)?
                .split(|c: char| !c.is_ascii_digit())
                .next()?
                .parse()
                .ok()?;
            Some((idx, parse_ssim(l)?))
        })
        .collect();
    found.sort_by_key(|f| f.0);
    found.into_iter().map(|f| f.1).collect()
}

/// Offset with the best score for offsets −1, 0, +1; 0 wins near-ties.
fn pick_offset(scores: [f64; 3]) -> i32 {
    let (best, score) = scores
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map(|(i, s)| (i as i32 - 1, *s))
        .unwrap_or((0, 0.0));
    if score - scores[1] < 0.002 { 0 } else { best }
}

/// What a sample is measured against.
#[derive(Clone, Copy)]
enum Reference<'a> {
    /// Source window, filtered on the fly like the encode.
    Source(Window),
    /// Pre-filtered lossless copy of the window.
    Prefiltered(&'a Path),
}

async fn remove_all(paths: &[PathBuf]) {
    for p in paths {
        let _ = tokio::fs::remove_file(p).await;
    }
}

/// Next CRF to measure in the search for the highest CRF whose score still
/// meets `threshold`, or `None` when the answer is known.
///
/// Scores fall roughly linearly with rising CRF. The search starts at the
/// profile CRF, then interpolates (with `slope_hint` until two points exist)
/// and falls back to bisection after three steps, so it never needs more
/// steps than `2 + log2(range)`. Every candidate lies strictly between the
/// best passing and the lowest failing CRF, so no CRF is measured twice.
/// If the best passing score is so close to the threshold that the next CRF
/// would miss it by the expected slope, the search stops early – the
/// conservative choice (slightly larger file, never lower quality).
pub fn next_crf(
    range: (u8, u8),
    start: u8,
    threshold: f64,
    measured: &[(u8, f64)],
    slope_hint: f64,
) -> Option<u8> {
    let (min, max) = (i32::from(range.0), i32::from(range.1));
    let best_pass = measured
        .iter()
        .filter(|m| m.1 >= threshold)
        .max_by_key(|m| m.0)
        .copied();
    let lowest_fail = measured
        .iter()
        .filter(|m| m.1 < threshold)
        .min_by_key(|m| m.0)
        .copied();
    let lo = best_pass.map_or(min, |p| i32::from(p.0) + 1);
    let hi = lowest_fail.map_or(max, |f| i32::from(f.0) - 1);
    if lo > hi {
        return None;
    }
    if measured.is_empty() {
        return Some(i32::from(start).clamp(lo, hi) as u8);
    }
    // Slope from the measurements nearest to the threshold, if usable.
    let mut by_distance: Vec<(u8, f64)> = measured.to_vec();
    by_distance.sort_by(|a, b| (a.1 - threshold).abs().total_cmp(&(b.1 - threshold).abs()));
    let slope = match (best_pass, lowest_fail) {
        (Some(p), Some(f)) => slope_between(p, f),
        _ if by_distance.len() >= 2 => slope_between(by_distance[0], by_distance[1]),
        _ => None,
    }
    .unwrap_or(slope_hint);
    if let Some(p) = best_pass
        && p.1 - threshold < slope * 0.5
    {
        return None;
    }
    if measured.len() >= 3 {
        return Some((lo + (hi - lo + 1) / 2) as u8);
    }
    let anchor = by_distance[0];
    let predicted = f64::from(anchor.0) + (anchor.1 - threshold) / slope;
    Some((predicted.floor() as i32).clamp(lo, hi) as u8)
}

/// Score drop per CRF step between two measurements (positive), if sane.
fn slope_between(a: (u8, f64), b: (u8, f64)) -> Option<f64> {
    if a.0 == b.0 {
        return None;
    }
    let s = (a.1 - b.1) / (f64::from(b.0) - f64::from(a.0));
    (s.is_finite() && s > 0.0).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows() {
        let w = sample_windows(Some(3600.0), 3, 20);
        assert_eq!(w.len(), 3);
        assert!((w[0].start_s - 890.0).abs() < 1e-6);
        assert!((w[2].start_s - 2690.0).abs() < 1e-6);
        let short = sample_windows(Some(30.0), 3, 20);
        assert_eq!(short.len(), 1);
        assert_eq!(short[0].duration_s, 20.0);
        let tiny = sample_windows(Some(5.0), 3, 20);
        assert_eq!(tiny[0].duration_s, 5.0);
        assert_eq!(sample_windows(None, 3, 20).len(), 1);
    }

    #[test]
    fn windows_start_between_frames() {
        let fps = 24000.0 / 1001.0;
        let w = snap_to_frame_gap(
            Window {
                start_s: 890.0,
                duration_s: 20.0,
            },
            Some(fps),
        );
        let pos = w.start_s * fps;
        assert!((pos - pos.floor() - 0.5).abs() < 1e-6, "{pos}");
        assert!((w.start_s - 890.0).abs() < 1.0 / fps);
        // Start of file and unknown rate stay untouched.
        let zero = Window {
            start_s: 0.0,
            duration_s: 5.0,
        };
        assert_eq!(snap_to_frame_gap(zero, Some(fps)), zero);
        assert_eq!(snap_to_frame_gap(w, None), w);
    }

    #[test]
    fn frame_offset_detection() {
        let log = "[Parsed_ssim_22 @ 0x1] SSIM Y:0.9 All:0.887198 (9.5)\n\
                   [Parsed_ssim_19 @ 0x2] SSIM Y:0.9 All:0.923321 (11.1)\n\
                   [Parsed_ssim_18 @ 0x3] SSIM Y:0.9 All:0.983864 (17.9)";
        let scores = parse_ssim_all(log);
        assert_eq!(scores, vec![0.983864, 0.923321, 0.887198]);
        assert_eq!(pick_offset([scores[0], scores[1], scores[2]]), -1);
        assert_eq!(pick_offset([0.91, 0.98, 0.92]), 0);
        assert_eq!(pick_offset([0.90, 0.92, 0.97]), 1);
        // near-tie (static scene): no shift
        assert_eq!(pick_offset([0.9901, 0.9900, 0.98]), 0);
    }

    #[test]
    fn parses_scores() {
        let ssim = "[Parsed_ssim_4 @ 0x55] SSIM Y:0.991 (20.4) U:0.995 (23.1) V:0.994 (22.6) All:0.992512 (21.25)";
        assert_eq!(parse_ssim(ssim), Some(0.992512));
        assert_eq!(
            parse_vmaf("[libvmaf @ 0x1] VMAF score: 95.432100"),
            Some(95.4321)
        );
        assert_eq!(parse_vmaf("VMAF score = 93.5"), Some(93.5));
        assert_eq!(parse_ssim("nothing"), None);
    }

    /// Runs the search against a synthetic score curve; returns
    /// (chosen CRF, measured CRFs).
    fn simulate(
        range: (u8, u8),
        start: u8,
        thr: f64,
        score: impl Fn(u8) -> f64,
    ) -> (Option<u8>, Vec<u8>) {
        let mut measured = Vec::new();
        while let Some(c) = next_crf(range, start, thr, &measured, 1.0) {
            assert!(
                measured.iter().all(|m: &(u8, f64)| m.0 != c),
                "CRF {c} measured twice"
            );
            measured.push((c, score(c)));
            assert!(measured.len() <= 6, "too many steps: {measured:?}");
        }
        let best = measured.iter().filter(|m| m.1 >= thr).map(|m| m.0).max();
        (best, measured.iter().map(|m| m.0).collect())
    }

    #[test]
    fn crf_search_finds_boundary_quickly() {
        // Linear curve matching the hint: VMAF 100 at CRF 18, -1 per step.
        let linear = |c: u8| 100.0 - f64::from(c - 18) * 1.0 + 0.3;
        // threshold 93 -> CRF 25 passes (93.3), 26 fails (92.3)
        let (best, steps) = simulate((20, 28), 23, 93.0, linear);
        assert_eq!(best, Some(25));
        assert!(steps.len() <= 3, "{steps:?}");

        // Steeper curve than the hint: still exact.
        let steep = |c: u8| 99.0 - f64::from(c - 18) * 1.7;
        let (best, steps) = simulate((18, 28), 23, 93.0, steep);
        // 21: 93.9 passes, 22: 92.2 fails
        assert_eq!(best, Some(21));
        assert!(steps.len() <= 4, "{steps:?}");
    }

    #[test]
    fn crf_search_is_exhaustive_on_brute_force() {
        // For many curves the result equals a brute-force scan (with the
        // early stop allowed to be one step conservative).
        for slope in [0.4, 0.8, 1.0, 1.5, 2.5] {
            for offset in [-6.0, -2.0, 0.0, 1.0, 3.0, 8.0] {
                let score = |c: u8| 95.0 + offset - f64::from(c) * slope + 23.0 * slope;
                let (best, steps) = simulate((18, 28), 23, 93.0, score);
                let truth = (18..=28).filter(|c| score(*c) >= 93.0).max();
                match (best, truth) {
                    (Some(b), Some(t)) => assert!(
                        b == t || b + 1 == t,
                        "slope {slope} off {offset}: {b} vs {t} {steps:?}"
                    ),
                    (b, t) => assert_eq!(b, t, "slope {slope} off {offset}: {steps:?}"),
                }
                assert!(steps.len() <= 2 + 4, "{steps:?}");
            }
        }
    }

    #[test]
    fn crf_search_handles_range_edges() {
        // Everything passes: ends at the top of the range.
        let (best, _) = simulate((20, 28), 23, 50.0, |_| 99.0);
        assert_eq!(best, Some(28));
        // Nothing passes: no result (caller falls back to crf_min).
        let (best, steps) = simulate((20, 28), 23, 99.5, |_| 90.0);
        assert_eq!(best, None);
        assert!(steps.contains(&20));
        // Start outside the range is clamped.
        assert_eq!(next_crf((20, 28), 30, 93.0, &[], 1.0), Some(28));
    }
}
