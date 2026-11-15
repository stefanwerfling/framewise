//! Stage 1: playback risks from the scan data.

use super::Finding;
use crate::core::config::HealthSettings;
use crate::media::model::{
    AudioCodec, DvCompatibility, HdrFormat, MediaFile, SubtitleCodec, SubtitleKind, VideoCodec,
};

/// What probably keeps `media` from playing directly on the device.
pub fn assess(media: &MediaFile, device: &HealthSettings) -> Vec<Finding> {
    let mut out = Vec::new();
    let Some(v) = media.primary_video() else {
        if media.audio.is_empty() {
            out.push(Finding::NoStreamInfo);
        }
        return out;
    };

    let codec_ok = match v.codec {
        VideoCodec::H264 | VideoCodec::Vp9 | VideoCodec::Vp8 => true,
        VideoCodec::Hevc => device.hevc,
        VideoCodec::Av1 => device.av1,
        VideoCodec::Mpeg2 | VideoCodec::Mpeg4 | VideoCodec::Vc1 | VideoCodec::Other(_) => false,
    };
    if !codec_ok {
        out.push(Finding::VideoCodec(v.codec.label().to_string()));
    } else if v.codec == VideoCodec::H264 && v.bit_depth.is_some_and(|d| d > 8) {
        out.push(Finding::TenBitH264);
    }
    if v.interlaced == Some(true) {
        out.push(Finding::Interlaced);
    }
    if let HdrFormat::DolbyVision(dv) = &v.hdr
        && !device.dolby_vision
        && dv.compatibility == DvCompatibility::None
    {
        out.push(Finding::DolbyVisionWithoutFallback(dv.profile));
    }
    if device.max_height > 0 && v.height > device.max_height + device.max_height / 20 {
        out.push(Finding::ResolutionTooHigh(v.height));
    }
    if let Some(rate) = media.overall_bitrate()
        && device.max_bitrate_mbps > 0
        && rate > u64::from(device.max_bitrate_mbps) * 1_000_000
    {
        out.push(Finding::BitrateTooHigh(rate));
    }

    // The track the player starts with.
    if let Some(a) = media
        .audio
        .iter()
        .find(|a| a.default)
        .or(media.audio.first())
    {
        let ok = match &a.codec {
            AudioCodec::Dts | AudioCodec::DtsHdMa | AudioCodec::DtsHdHra => device.dts,
            AudioCodec::TrueHd => device.truehd,
            AudioCodec::Other(_) => false,
            _ => true,
        };
        if !ok {
            out.push(Finding::AudioCodec(a.codec.label().to_string()));
        }
    }

    // Only subtitles that switch on by themselves matter.
    if let Some(s) = media.subtitles.iter().find(|s| {
        (s.default || s.forced)
            && match s.kind() {
                SubtitleKind::Bitmap => !device.image_subtitles,
                _ => {
                    matches!(s.codec, SubtitleCodec::Ass | SubtitleCodec::Ssa)
                        && !device.ass_subtitles
                }
            }
    }) {
        out.push(Finding::Subtitles(s.codec.label().to_string()));
    }

    let ext = media.extension.to_ascii_lowercase();
    if matches!(
        ext.as_str(),
        "avi" | "wmv" | "asf" | "flv" | "mpg" | "mpeg" | "vob" | "divx" | "rm" | "rmvb"
    ) {
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
    fn typical_android_tv_risks() {
        let device = HealthSettings::default();

        let good = media(
            "mkv",
            1_000_000_000,
            3600.0,
            vec![video("hevc", 3840, 2160)],
            vec![audio("eac3", None, 6, "eng")],
            vec![],
        );
        assert!(
            assess(&good, &device).is_empty(),
            "{:?}",
            assess(&good, &device)
        );

        let mut bad = media(
            "avi",
            40_000_000_000,
            3600.0,
            vec![video("vc1", 1920, 1080)],
            vec![audio("dts", None, 6, "ger")],
            vec![subtitle("hdmv_pgs_subtitle", "ger")],
        );
        bad.video[0].interlaced = Some(true);
        bad.video[0].variable_frame_rate = true;
        bad.subtitles[0].forced = true;
        let found = assess(&bad, &device);
        for expected in [
            Finding::VideoCodec("VC-1".into()),
            Finding::Interlaced,
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

        // The same file on a device that plays everything.
        let all = HealthSettings {
            dts: true,
            image_subtitles: true,
            max_bitrate_mbps: 0,
            ..device.clone()
        };
        assert_eq!(assess(&bad, &all).len(), 4, "{:?}", assess(&bad, &all));

        let mut dv5 = good.clone();
        dv5.video[0].hdr = HdrFormat::DolbyVision(DolbyVisionInfo {
            profile: Some(5),
            level: Some(6),
            compatibility: DvCompatibility::None,
            has_enhancement_layer: false,
        });
        assert_eq!(
            assess(&dv5, &device),
            [Finding::DolbyVisionWithoutFallback(Some(5))]
        );
    }
}
