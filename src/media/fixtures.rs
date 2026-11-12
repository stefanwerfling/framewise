//! Builders for test media (only compiled for tests).

use super::classify;
use super::model::*;

pub fn video(codec: &str, width: u32, height: u32) -> VideoStream {
    VideoStream {
        index: 0,
        type_index: 0,
        codec: classify::video_codec(codec),
        codec_name: codec.into(),
        profile: None,
        width,
        height,
        sample_aspect_ratio: Ratio::new(1, 1),
        display_aspect_ratio: None,
        frame_rate: Ratio::new(24000, 1001),
        bit_rate: None,
        pix_fmt: Some("yuv420p".into()),
        bit_depth: Some(8),
        color: ColorInfo::default(),
        hdr: HdrFormat::Sdr,
        rotation: None,
        interlaced: Some(false),
        default: true,
        attached_pic: false,
        crop: None,
        hdr_metadata: None,
    }
}

pub fn audio(codec: &str, profile: Option<&str>, channels: u32, lang: &str) -> AudioStream {
    let c = classify::audio_codec(codec, profile);
    AudioStream {
        index: 0,
        type_index: 0,
        atmos: classify::detect_atmos(&c, profile, None),
        dts_x: classify::detect_dts_x(&c, profile, None),
        codec: c,
        codec_name: codec.into(),
        profile: profile.map(Into::into),
        language: Some(lang.into()),
        title: None,
        bit_rate: None,
        channels: Some(channels),
        channel_layout: None,
        sample_rate: Some(48000),
        default: false,
        forced: false,
        commentary: false,
        visual_impaired: false,
    }
}

pub fn subtitle(codec: &str, lang: &str) -> SubtitleStream {
    SubtitleStream {
        index: 0,
        type_index: 0,
        codec: classify::subtitle_codec(codec),
        codec_name: codec.into(),
        language: Some(lang.into()),
        title: None,
        default: false,
        forced: false,
        hearing_impaired: false,
    }
}

/// Assemble a media file and fix up stream indices.
pub fn media(
    ext: &str,
    size_bytes: u64,
    duration_s: f64,
    video: Vec<VideoStream>,
    mut audio: Vec<AudioStream>,
    mut subtitles: Vec<SubtitleStream>,
) -> MediaFile {
    let mut idx = video.len() as u32;
    for (i, a) in audio.iter_mut().enumerate() {
        a.type_index = i as u32;
        a.index = idx;
        idx += 1;
    }
    if !audio.iter().any(|a| a.default)
        && let Some(first) = audio.first_mut()
    {
        first.default = true;
    }
    for (i, s) in subtitles.iter_mut().enumerate() {
        s.type_index = i as u32;
        s.index = idx;
        idx += 1;
    }
    MediaFile {
        absolute_path: format!("/src/Movie.{ext}").into(),
        relative_path: format!("Movie.{ext}").into(),
        file_name: format!("Movie.{ext}"),
        extension: ext.into(),
        size_bytes,
        modified_unix: 0,
        container: ContainerInfo {
            format_name: ext.into(),
            duration_s: Some(duration_s),
            bit_rate: Some((size_bytes as f64 * 8.0 / duration_s) as u64),
            stream_count: idx,
            chapters: 0,
            title: None,
        },
        video,
        audio,
        subtitles,
        other_streams: vec![],
    }
}
