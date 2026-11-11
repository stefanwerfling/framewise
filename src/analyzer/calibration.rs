//! Calibration of the size estimate from real results.
//!
//! The fixed bitrate model in `encoding::plan` is a rule of thumb. Every
//! finished re-encode stores how far the video part of its estimate was off
//! (`SizeSample`); the median of these ratios per target codec then corrects
//! new estimates. Results remain **estimates** – the calibration only makes
//! them track the user's real library and settings more closely.

use serde::{Deserialize, Serialize};

use crate::encoding::plan::{self, EncodePlan, VideoAction};
use crate::encoding::profile::TargetVideoCodec;
use crate::media::model::MediaFile;
use crate::queue::job::{HistoryEntry, JobState};

/// Samples needed before a codec's estimate is corrected.
pub const MIN_SAMPLES: usize = 2;
/// Only the most recent results count (settings and content drift).
const MAX_SAMPLES: usize = 50;
/// Guard against absurd outliers (e.g. a broken result).
const FACTOR_RANGE: (f64, f64) = (0.2, 5.0);

/// Estimate vs. reality of one finished re-encode.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SizeSample {
    pub codec: TargetVideoCodec,
    /// Estimated bytes of the video stream (uncalibrated model).
    pub estimated_video: u64,
    /// Estimated bytes of everything else (audio, overhead).
    pub estimated_other: u64,
}

impl SizeSample {
    /// Sample for a plan that re-encodes video; `None` for copies.
    pub fn from_plan(media: &MediaFile, plan: &EncodePlan) -> Option<Self> {
        let video = plan.video.as_ref()?;
        let VideoAction::Encode { codec, .. } = video.action else {
            return None;
        };
        let parts = plan::estimate_parts(media, Some(video), &plan.audio)?;
        (parts.video > 0).then_some(Self {
            codec,
            estimated_video: parts.video,
            estimated_other: parts.other,
        })
    }

    /// Ratio actual / estimated for the video part.
    pub fn factor(&self, result_size: u64) -> Option<f64> {
        let actual_video = result_size.checked_sub(self.estimated_other)?;
        if actual_video == 0 || self.estimated_video == 0 {
            return None;
        }
        let f = actual_video as f64 / self.estimated_video as f64;
        (FACTOR_RANGE.0..=FACTOR_RANGE.1).contains(&f).then_some(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CodecFactor {
    pub factor: f64,
    pub samples: usize,
}

/// Correction factors per target codec.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SizeCalibration {
    pub h264: Option<CodecFactor>,
    pub hevc: Option<CodecFactor>,
}

impl SizeCalibration {
    /// Builds the calibration from the history (newest entries first).
    pub fn from_history(history: &[HistoryEntry]) -> Self {
        let mut h264 = Vec::new();
        let mut hevc = Vec::new();
        for h in history {
            if h.state != JobState::Done || !h.validation_passed {
                continue;
            }
            let (Some(sample), Some(size)) = (h.size_sample, h.result_size) else {
                continue;
            };
            let Some(f) = sample.factor(size) else {
                continue;
            };
            let list = match sample.codec {
                TargetVideoCodec::H264 => &mut h264,
                TargetVideoCodec::Hevc => &mut hevc,
            };
            if list.len() < MAX_SAMPLES {
                list.push(f);
            }
        }
        Self {
            h264: codec_factor(h264),
            hevc: codec_factor(hevc),
        }
    }

    pub fn factor(&self, codec: TargetVideoCodec) -> Option<CodecFactor> {
        match codec {
            TargetVideoCodec::H264 => self.h264,
            TargetVideoCodec::Hevc => self.hevc,
        }
    }

    /// Corrects `plan.estimated_size`; returns the number of results the
    /// correction is based on (`None` = model estimate unchanged).
    pub fn apply(&self, media: &MediaFile, plan: &mut EncodePlan) -> Option<usize> {
        let sample = SizeSample::from_plan(media, plan)?;
        let cf = self.factor(sample.codec)?;
        let video = (sample.estimated_video as f64 * cf.factor) as u64;
        plan.estimated_size = Some(video + sample.estimated_other);
        Some(cf.samples)
    }
}

fn codec_factor(mut factors: Vec<f64>) -> Option<CodecFactor> {
    if factors.len() < MIN_SAMPLES {
        return None;
    }
    factors.sort_by(f64::total_cmp);
    let n = factors.len();
    let median = if n % 2 == 1 {
        factors[n / 2]
    } else {
        (factors[n / 2 - 1] + factors[n / 2]) / 2.0
    };
    Some(CodecFactor {
        factor: median,
        samples: n,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(codec: TargetVideoCodec, est_video: u64, other: u64, result: u64) -> HistoryEntry {
        HistoryEntry {
            id: 0,
            job_id: 1,
            source: "a.mkv".into(),
            target: "a.mp4".into(),
            source_size: 10_000,
            result_size: Some(result),
            profile_id: "universal".into(),
            encoder: None,
            encode_seconds: None,
            media_duration_s: None,
            quality: None,
            validation_passed: true,
            validation: None,
            state: JobState::Done,
            error: None,
            warnings: Vec::new(),
            finished_unix: 0,
            size_sample: Some(SizeSample {
                codec,
                estimated_video: est_video,
                estimated_other: other,
            }),
        }
    }

    #[test]
    fn factor_uses_video_part_only() {
        let s = SizeSample {
            codec: TargetVideoCodec::H264,
            estimated_video: 1000,
            estimated_other: 500,
        };
        // 2500 total, 500 audio -> video 2000 = 2x estimate.
        assert_eq!(s.factor(2500), Some(2.0));
        // Result smaller than the audio estimate: unusable.
        assert_eq!(s.factor(400), None);
        // Outlier beyond the allowed range.
        assert_eq!(s.factor(500 + 10_000), None);
    }

    #[test]
    fn needs_min_samples_and_uses_median() {
        let one = vec![entry(TargetVideoCodec::H264, 1000, 0, 2000)];
        assert_eq!(SizeCalibration::from_history(&one).h264, None);

        let hist = vec![
            entry(TargetVideoCodec::H264, 1000, 0, 1500),
            entry(TargetVideoCodec::H264, 1000, 0, 4000),
            entry(TargetVideoCodec::H264, 1000, 0, 2000),
            entry(TargetVideoCodec::Hevc, 1000, 0, 500),
        ];
        let cal = SizeCalibration::from_history(&hist);
        let f = cal.h264.unwrap();
        assert_eq!(f.samples, 3);
        assert!((f.factor - 2.0).abs() < 1e-9);
        // A single HEVC result is not enough.
        assert_eq!(cal.hevc, None);
    }

    #[test]
    fn failed_or_unvalidated_results_are_ignored() {
        let mut a = entry(TargetVideoCodec::H264, 1000, 0, 3000);
        a.state = JobState::Failed;
        let mut b = entry(TargetVideoCodec::H264, 1000, 0, 3000);
        b.validation_passed = false;
        let c = entry(TargetVideoCodec::H264, 1000, 0, 1000);
        let cal = SizeCalibration::from_history(&[a, b, c]);
        assert_eq!(cal.h264, None);
    }
}
