//! Classification helpers: codec families, Atmos/DTS:X hints, HDR detection.

use super::model::{
    AudioCodec, ColorInfo, Detection, DetectionSource, DolbyVisionInfo, DvCompatibility, HdrFormat,
    SubtitleCodec, VideoCodec,
};

pub fn video_codec(name: &str) -> VideoCodec {
    match name {
        "h264" => VideoCodec::H264,
        "hevc" | "h265" => VideoCodec::Hevc,
        "av1" => VideoCodec::Av1,
        "vp9" => VideoCodec::Vp9,
        "vp8" => VideoCodec::Vp8,
        "mpeg2video" | "mpeg1video" => VideoCodec::Mpeg2,
        "mpeg4" | "msmpeg4v3" | "msmpeg4v2" => VideoCodec::Mpeg4,
        "vc1" | "wmv3" => VideoCodec::Vc1,
        other => VideoCodec::Other(other.to_string()),
    }
}

pub fn audio_codec(name: &str, profile: Option<&str>) -> AudioCodec {
    let profile = profile.unwrap_or_default();
    match name {
        "aac" | "aac_latm" => AudioCodec::Aac,
        "ac3" => AudioCodec::Ac3,
        "eac3" => AudioCodec::Eac3,
        "dts" => {
            if profile.contains("MA") || profile.contains("DTS:X") {
                AudioCodec::DtsHdMa
            } else if profile.contains("HRA") || profile.contains("HD") {
                AudioCodec::DtsHdHra
            } else {
                AudioCodec::Dts
            }
        }
        "truehd" | "mlp" => AudioCodec::TrueHd,
        "flac" => AudioCodec::Flac,
        "opus" => AudioCodec::Opus,
        "vorbis" => AudioCodec::Vorbis,
        "mp3" | "mp2" => AudioCodec::Mp3,
        "alac" => AudioCodec::Alac,
        n if n.starts_with("pcm_") => AudioCodec::Pcm,
        other => AudioCodec::Other(other.to_string()),
    }
}

/// Dolby Atmos can be carried by TrueHD and E-AC3 (JOC). Newer ffprobe
/// versions report it in the profile; otherwise only the track title gives
/// a hint, which is marked as "probable".
pub fn detect_atmos(codec: &AudioCodec, profile: Option<&str>, title: Option<&str>) -> Detection {
    if !matches!(codec, AudioCodec::TrueHd | AudioCodec::Eac3) {
        return Detection::No;
    }
    if let Some(p) = profile.filter(|p| p.to_ascii_lowercase().contains("atmos")) {
        return Detection::Confirmed(DetectionSource::CodecProfile(p.to_string()));
    }
    if title.is_some_and(|t| t.to_ascii_lowercase().contains("atmos")) {
        return Detection::Probable(DetectionSource::TrackTitle);
    }
    Detection::No
}

pub fn detect_dts_x(codec: &AudioCodec, profile: Option<&str>, title: Option<&str>) -> Detection {
    if !codec.is_dts_family() {
        return Detection::No;
    }
    if let Some(p) = profile.filter(|p| p.contains("DTS:X")) {
        return Detection::Confirmed(DetectionSource::CodecProfile(p.to_string()));
    }
    if title.is_some_and(|t| {
        let t = t.to_ascii_lowercase();
        t.contains("dts:x") || t.contains("dts-x") || t.contains("dtsx")
    }) {
        return Detection::Probable(DetectionSource::TrackTitle);
    }
    Detection::No
}

pub fn subtitle_codec(name: &str) -> SubtitleCodec {
    match name {
        "subrip" | "srt" => SubtitleCodec::Subrip,
        "ass" => SubtitleCodec::Ass,
        "ssa" => SubtitleCodec::Ssa,
        "webvtt" => SubtitleCodec::WebVtt,
        "mov_text" | "tx3g" => SubtitleCodec::MovText,
        "hdmv_pgs_subtitle" | "pgssub" => SubtitleCodec::Pgs,
        "dvd_subtitle" | "dvdsub" => SubtitleCodec::VobSub,
        "dvb_subtitle" | "dvbsub" => SubtitleCodec::DvbSub,
        "text" => SubtitleCodec::Subrip,
        other => SubtitleCodec::Other(other.to_string()),
    }
}

/// Bit depth from `bits_per_raw_sample` or the pixel format name.
pub fn bit_depth(raw: Option<u8>, pix_fmt: Option<&str>) -> Option<u8> {
    if let Some(b) = raw.filter(|b| *b > 0) {
        return Some(b);
    }
    let p = pix_fmt?;
    Some(if p.contains("12") {
        12
    } else if p.contains("10") || p == "p010le" {
        10
    } else {
        8
    })
}

/// Inputs for HDR classification.
#[derive(Debug, Default, Clone)]
pub struct HdrSignals {
    pub dovi: Option<DolbyVisionInfo>,
    /// Codec tag like `dvh1`, `dvhe`, `dav1`.
    pub codec_tag: Option<String>,
    /// HDR10+ dynamic metadata (SMPTE ST 2094-40) found in frame side data.
    pub hdr10_plus: bool,
    pub mastering_display: bool,
}

/// Determine the dynamic range of a video stream.
///
/// Order: Dolby Vision (configuration record / codec tag) → transfer
/// characteristics (PQ / HLG) → HDR10+ refinement. Untagged streams are
/// treated as SDR, which is what players do as well.
pub fn detect_hdr(color: &ColorInfo, signals: &HdrSignals) -> HdrFormat {
    if let Some(dv) = &signals.dovi {
        return HdrFormat::DolbyVision(dv.clone());
    }
    if let Some(tag) = signals.codec_tag.as_deref()
        && matches!(tag, "dvh1" | "dvhe" | "dav1" | "dva1" | "dvav")
    {
        return HdrFormat::DolbyVision(DolbyVisionInfo::default());
    }
    match color.transfer.as_deref() {
        Some("smpte2084") => {
            if signals.hdr10_plus {
                HdrFormat::Hdr10Plus
            } else {
                HdrFormat::Hdr10
            }
        }
        Some("arib-std-b67") => HdrFormat::Hlg,
        _ if signals.mastering_display && color.primaries.as_deref() == Some("bt2020") => {
            HdrFormat::Unknown
        }
        _ => HdrFormat::Sdr,
    }
}

pub fn dv_compatibility(id: Option<u8>, profile: Option<u8>) -> DvCompatibility {
    match id {
        Some(0) => DvCompatibility::None,
        Some(1) | Some(6) => DvCompatibility::Hdr10,
        Some(2) => DvCompatibility::Sdr,
        Some(4) => DvCompatibility::Hlg,
        _ if profile == Some(5) => DvCompatibility::None,
        _ => DvCompatibility::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_families() {
        assert_eq!(audio_codec("dts", Some("DTS-HD MA")), AudioCodec::DtsHdMa);
        assert_eq!(audio_codec("dts", Some("DTS-HD HRA")), AudioCodec::DtsHdHra);
        assert_eq!(audio_codec("dts", Some("DTS")), AudioCodec::Dts);
        assert_eq!(audio_codec("dts", None), AudioCodec::Dts);
        assert_eq!(audio_codec("pcm_s24le", None), AudioCodec::Pcm);
        assert_eq!(audio_codec("truehd", None), AudioCodec::TrueHd);
    }

    #[test]
    fn atmos_and_dtsx() {
        let a = detect_atmos(
            &AudioCodec::TrueHd,
            Some("Dolby TrueHD + Dolby Atmos"),
            None,
        );
        assert!(matches!(a, Detection::Confirmed(_)));
        let a = detect_atmos(&AudioCodec::Eac3, None, Some("English Atmos 7.1"));
        assert!(matches!(a, Detection::Probable(_)));
        assert_eq!(
            detect_atmos(&AudioCodec::Aac, None, Some("Atmos")),
            Detection::No
        );
        let x = detect_dts_x(&AudioCodec::DtsHdMa, Some("DTS:X"), None);
        assert!(matches!(x, Detection::Confirmed(_)));
        let x = detect_dts_x(&AudioCodec::DtsHdMa, None, Some("DTS-X 7.1"));
        assert!(matches!(x, Detection::Probable(_)));
    }

    #[test]
    fn hdr_detection() {
        let pq = ColorInfo {
            transfer: Some("smpte2084".into()),
            primaries: Some("bt2020".into()),
            ..Default::default()
        };
        assert_eq!(detect_hdr(&pq, &HdrSignals::default()), HdrFormat::Hdr10);
        let plus = HdrSignals {
            hdr10_plus: true,
            ..Default::default()
        };
        assert_eq!(detect_hdr(&pq, &plus), HdrFormat::Hdr10Plus);
        let hlg = ColorInfo {
            transfer: Some("arib-std-b67".into()),
            ..Default::default()
        };
        assert_eq!(detect_hdr(&hlg, &HdrSignals::default()), HdrFormat::Hlg);
        assert_eq!(
            detect_hdr(&ColorInfo::default(), &HdrSignals::default()),
            HdrFormat::Sdr
        );
        let dv = HdrSignals {
            codec_tag: Some("dvh1".into()),
            ..Default::default()
        };
        assert!(matches!(detect_hdr(&pq, &dv), HdrFormat::DolbyVision(_)));
    }

    #[test]
    fn bit_depths() {
        assert_eq!(bit_depth(Some(10), Some("yuv420p")), Some(10));
        assert_eq!(bit_depth(None, Some("yuv420p10le")), Some(10));
        assert_eq!(bit_depth(None, Some("yuv420p")), Some(8));
        assert_eq!(bit_depth(None, None), None);
    }
}
