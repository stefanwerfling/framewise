//! Rule based recommendation engine.
//!
//! The engine reuses the encode planner so that recommendation and actual
//! processing can never disagree. Savings are **estimates** and are always
//! labelled as such in the GUI.

use serde::{Deserialize, Serialize};

use crate::analyzer::calibration::SizeCalibration;
use crate::encoding::plan::{
    self, AudioAction, EncodePlan, PlanCapabilities, SubtitleAction, VideoAction, VideoReason,
};
use crate::encoding::profile::EncodingProfile;
use crate::media::model::{MediaFile, SubtitleCodec, VideoStream};
use crate::tr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Recommendation {
    /// Compatible and efficient – nothing to do.
    AlreadyOptimal,
    /// Only container/audio/subtitles need changes; video is copied.
    Remux,
    /// Video needs re-encoding.
    Transcode,
    /// Cannot be processed automatically – needs user attention.
    Review,
}

impl Recommendation {
    pub fn label(self) -> &'static str {
        match self {
            Recommendation::AlreadyOptimal => tr!("Already optimal"),
            Recommendation::Remux => tr!("Remux (copy video)"),
            Recommendation::Transcode => tr!("Re-encode"),
            Recommendation::Review => tr!("Review"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Compatibility {
    /// Direct play on practically all Plex/Jellyfin clients.
    Compatible,
    /// Video fine, but container/audio/subtitles limit direct play.
    Partial,
    /// Video format limits direct play on many clients.
    Incompatible,
}

impl Compatibility {
    pub fn label(self) -> &'static str {
        match self {
            Compatibility::Compatible => tr!("Compatible"),
            Compatibility::Partial => tr!("Partial"),
            Compatibility::Incompatible => tr!("Incompatible"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Analysis {
    pub recommendation: Recommendation,
    pub compatibility: Compatibility,
    pub plan: EncodePlan,
    /// Video bits per pixel per frame.
    pub bits_per_pixel: Option<f64>,
    /// Estimated result size (estimate!).
    pub estimated_size: Option<u64>,
    /// Estimated savings in bytes (estimate!, never negative).
    pub estimated_savings: Option<u64>,
    /// 0–100, higher = more worthwhile.
    pub potential_score: u8,
    /// Number of real results the size estimate was calibrated with
    /// (`None` = fixed model only).
    #[serde(default)]
    pub calibrated_from: Option<usize>,
}

impl Analysis {
    pub fn savings_ratio(&self, source_size: u64) -> Option<f64> {
        self.estimated_savings
            .filter(|_| source_size > 0)
            .map(|s| s as f64 / source_size as f64)
    }
}

fn video_compatible(reasons: &[VideoReason]) -> bool {
    reasons.iter().all(|r| {
        matches!(
            r,
            VideoReason::HighBitrate { .. } | VideoReason::ProfileForcesEncode
        )
    })
}

pub fn analyze(media: &MediaFile, profile: &EncodingProfile, caps: &PlanCapabilities) -> Analysis {
    analyze_calibrated(media, profile, caps, &SizeCalibration::default())
}

/// Like [`analyze`], with the size estimate corrected from real results.
pub fn analyze_calibrated(
    media: &MediaFile,
    profile: &EncodingProfile,
    caps: &PlanCapabilities,
    calibration: &SizeCalibration,
) -> Analysis {
    let mut plan = plan::build_plan(media, profile, caps);
    let calibrated_from = calibration.apply(media, &mut plan);

    let bits_per_pixel = media.primary_video().and_then(|v: &VideoStream| {
        let br = media.video_bitrate()?;
        plan::bits_per_pixel(
            br,
            crate::analyzer::resolution::Dimensions::new(v.width, v.height),
            v.fps()?,
        )
    });

    let video_ok = plan
        .video
        .as_ref()
        .is_some_and(|v| video_compatible(&v.reasons));
    let audio_ok = plan.audio.iter().all(|a| {
        a.action == AudioAction::Copy
            && media
                .audio
                .get(a.source_type_index as usize)
                .is_some_and(|s| {
                    !s.codec.is_dts_family()
                        && !matches!(s.codec, crate::media::model::AudioCodec::Alac)
                })
    }) && !plan
        .audio
        .iter()
        .any(|a| a.purpose == plan::AudioPurpose::CompatibilityStereo);
    let subs_ok = media
        .subtitles
        .iter()
        .all(|s| s.codec == SubtitleCodec::MovText);
    let is_mp4 = media.extension == "mp4";

    let compatibility = if !video_ok {
        Compatibility::Incompatible
    } else if is_mp4 && audio_ok && subs_ok {
        Compatibility::Compatible
    } else {
        Compatibility::Partial
    };

    let video_copied = matches!(
        plan.video.as_ref().map(|v| &v.action),
        Some(VideoAction::Copy)
    );
    let nothing_to_convert = plan.audio.iter().all(|a| a.action == AudioAction::Copy)
        && plan
            .subtitles
            .iter()
            .all(|s| s.action == SubtitleAction::MovText && s.codec == SubtitleCodec::MovText);

    let recommendation = if plan.is_blocked() {
        Recommendation::Review
    } else if video_copied && compatibility == Compatibility::Compatible && nothing_to_convert {
        Recommendation::AlreadyOptimal
    } else if video_copied {
        Recommendation::Remux
    } else {
        Recommendation::Transcode
    };

    let estimated_size = if recommendation == Recommendation::AlreadyOptimal {
        Some(media.size_bytes)
    } else {
        plan.estimated_size
    };
    let estimated_savings = estimated_size.map(|e| media.size_bytes.saturating_sub(e));

    let ratio = estimated_savings
        .filter(|_| media.size_bytes > 0)
        .map_or(0.0, |s| s as f64 / media.size_bytes as f64);
    let compat_bonus = match compatibility {
        Compatibility::Compatible => 0.0,
        Compatibility::Partial => 10.0,
        Compatibility::Incompatible => 20.0,
    };
    let potential_score = match recommendation {
        Recommendation::AlreadyOptimal => 0,
        _ => (ratio * 80.0 + compat_bonus).round().clamp(0.0, 100.0) as u8,
    };

    Analysis {
        recommendation,
        compatibility,
        plan,
        bits_per_pixel,
        estimated_size,
        estimated_savings,
        potential_score,
        calibrated_from: calibrated_from
            .filter(|_| recommendation != Recommendation::AlreadyOptimal),
    }
}

#[cfg(test)]
mod tests;
