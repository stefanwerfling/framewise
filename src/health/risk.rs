//! Stage 1: playback risks from the scan data.
//!
//! The yardstick is not one device but the most compatible format – what
//! every common TV player plays directly, the plain 1080p Fire TV stick
//! included (Amazon device specifications, Jellyfin codec tables):
//!
//! * MP4/MKV, H.264 (8 bit) or HEVC, up to 1080p, at most 20 Mbit/s,
//!   progressive, constant frame rate;
//! * SDR, HDR10 or HLG; Dolby Vision only with an HDR10/SDR base;
//! * audio AAC, AC3, E-AC3, MP3, FLAC or Opus (DTS/TrueHD only with a
//!   receiver, Vorbis/ALAC not in the Jellyfin Android TV app);
//! * subtitles that switch on by themselves as text (SRT/mov_text), not as
//!   image (PGS/VobSub) or ASS – those are burned in by the server.

use super::Finding;
use crate::media::model::{
    AudioCodec, DvCompatibility, HdrFormat, MediaFile, SubtitleCodec, SubtitleKind, VideoCodec,
};

/// Highest resolution every player decodes.
pub const MAX_HEIGHT: u32 = 1080;
/// Highest video bitrate every player decodes (bits/s).
pub const MAX_BITRATE: u64 = 20_000_000;

/// What probably keeps `media` from playing directly everywhere.
pub fn assess(media: &MediaFile) -> Vec<Finding> {
    let mut out = Vec::new();
    let Some(v) = media.primary_video() else {
        if media.audio.is_empty() {
            out.push(Finding::NoStreamInfo);
        }
        return out;
    };

    match v.codec {
        VideoCodec::H264 | VideoCodec::Hevc | VideoCodec::Vp9 => {}
        _ => out.push(Finding::VideoCodec(v.codec.label().to_string())),
    }
    if v.codec == VideoCodec::H264 && v.bit_depth.is_some_and(|d| d > 8) {
        out.push(Finding::TenBitH264);
    }
    if v.interlaced == Some(true) {
        out.push(Finding::Interlaced);
    }
    if let HdrFormat::DolbyVision(dv) = &v.hdr
        && (dv.compatibility == DvCompatibility::None || dv.profile == Some(7))
    {
        out.push(Finding::DolbyVisionWithoutFallback(dv.profile));
    }
    // A little tolerance for 1920×1088 and similar.
    if v.height > MAX_HEIGHT + MAX_HEIGHT / 20 {
        out.push(Finding::ResolutionTooHigh(v.height));
    }
    if let Some(rate) = media.video_bitrate().filter(|r| *r > MAX_BITRATE) {
        out.push(Finding::BitrateTooHigh(rate));
    }

    // The track the player starts with.
    if let Some(a) = media
        .audio
        .iter()
        .find(|a| a.default)
        .or(media.audio.first())
    {
        let ok = matches!(
            a.codec,
            AudioCodec::Aac
                | AudioCodec::Ac3
                | AudioCodec::Eac3
                | AudioCodec::Mp3
                | AudioCodec::Flac
                | AudioCodec::Opus
                | AudioCodec::Pcm
        );
        if !ok {
            out.push(Finding::AudioCodec(a.codec.label().to_string()));
        }
    }

    // Only subtitles that switch on by themselves matter.
    if let Some(s) = media.subtitles.iter().find(|s| {
        (s.default || s.forced)
            && (s.kind() == SubtitleKind::Bitmap
                || matches!(s.codec, SubtitleCodec::Ass | SubtitleCodec::Ssa))
    }) {
        out.push(Finding::Subtitles(s.codec.label().to_string()));
    }

    let ext = media.extension.to_ascii_lowercase();
    if !ext.is_empty() && !matches!(ext.as_str(), "mp4" | "m4v" | "mkv" | "webm" | "mov") {
        out.push(Finding::Container(ext.to_uppercase()));
    }
    if v.variable_frame_rate {
        out.push(Finding::VariableFrameRate);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::fixtures::*;
    use crate::media::model::DolbyVisionInfo;

    #[test]
    fn deviations_from_the_most_compatible_format() {
        let good = media(
            "mkv",
            4_000_000_000,
            3600.0,
            vec![video("hevc", 1920, 800)],
            vec![audio("eac3", None, 6, "ger")],
            vec![subtitle("subrip", "ger")],
        );
        assert!(assess(&good).is_empty(), "{:?}", assess(&good));

        let mut bad = media(
            "avi",
            40_000_000_000,
            3600.0,
            vec![video("vc1", 3840, 2160)],
            vec![audio("dts", None, 6, "ger")],
            vec![subtitle("hdmv_pgs_subtitle", "ger")],
        );
        bad.video[0].interlaced = Some(true);
        bad.video[0].variable_frame_rate = true;
        bad.subtitles[0].forced = true;
        let found = assess(&bad);
        for expected in [
            Finding::VideoCodec("VC-1".into()),
            Finding::Interlaced,
            Finding::ResolutionTooHigh(2160),
            Finding::AudioCodec("DTS".into()),
            Finding::Subtitles("PGS".into()),
            Finding::Container("AVI".into()),
            Finding::VariableFrameRate,
        ] {
            assert!(
                found.contains(&expected),
                "{expected:?} missing in {found:?}"
            );
        }
        assert!(
            found
                .iter()
                .any(|f| matches!(f, Finding::BitrateTooHigh(_)))
        );

        let vorbis = media(
            "mkv",
            1_000_000_000,
            3600.0,
            vec![video("h264", 1280, 720)],
            vec![audio("vorbis", None, 2, "eng")],
            vec![],
        );
        assert_eq!(assess(&vorbis), [Finding::AudioCodec("Vorbis".into())]);

        let dv = |profile, compatibility| {
            let mut m = good.clone();
            m.video[0].hdr = HdrFormat::DolbyVision(DolbyVisionInfo {
                profile: Some(profile),
                level: Some(6),
                compatibility,
                has_enhancement_layer: false,
            });
            assess(&m)
        };
        assert_eq!(
            dv(5, DvCompatibility::None),
            [Finding::DolbyVisionWithoutFallback(Some(5))]
        );
        assert_eq!(
            dv(7, DvCompatibility::Hdr10),
            [Finding::DolbyVisionWithoutFallback(Some(7))]
        );
        assert!(dv(8, DvCompatibility::Hdr10).is_empty(), "HDR10 base plays");
    }
}
