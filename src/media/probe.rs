//! ffprobe invocation and robust mapping of its JSON output to the domain model.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use super::classify::{self, HdrSignals};
use super::model::{
    AudioStream, ColorInfo, ContainerInfo, DolbyVisionInfo, MediaFile, Ratio, SubtitleStream,
    VideoStream,
};
use crate::core::error::{Error, Result};
use crate::core::tools::run_capture;
use crate::{tr, trf};

const PROBE_TIMEOUT: Duration = Duration::from_secs(120);

/// Raw ffprobe JSON (`-show_format -show_streams -show_chapters`).
#[derive(Debug, Deserialize, Default)]
pub struct ProbeOutput {
    #[serde(default)]
    pub streams: Vec<ProbeStream>,
    #[serde(default)]
    pub format: Option<ProbeFormat>,
    #[serde(default)]
    pub chapters: Vec<Value>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ProbeFormat {
    pub format_name: Option<String>,
    pub duration: Option<String>,
    pub bit_rate: Option<String>,
    pub nb_streams: Option<u32>,
    #[serde(default)]
    pub tags: HashMap<String, String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ProbeStream {
    pub index: u32,
    pub codec_type: Option<String>,
    pub codec_name: Option<String>,
    pub profile: Option<String>,
    pub codec_tag_string: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub sample_aspect_ratio: Option<String>,
    pub display_aspect_ratio: Option<String>,
    pub pix_fmt: Option<String>,
    pub color_range: Option<String>,
    pub color_space: Option<String>,
    pub color_transfer: Option<String>,
    pub color_primaries: Option<String>,
    pub field_order: Option<String>,
    pub r_frame_rate: Option<String>,
    pub avg_frame_rate: Option<String>,
    pub bit_rate: Option<String>,
    pub bits_per_raw_sample: Option<String>,
    pub sample_rate: Option<String>,
    pub channels: Option<u32>,
    pub channel_layout: Option<String>,
    #[serde(default)]
    pub disposition: HashMap<String, i64>,
    #[serde(default)]
    pub tags: HashMap<String, String>,
    #[serde(default)]
    pub side_data_list: Vec<Value>,
}

impl ProbeStream {
    /// Case-insensitive tag lookup (MKV writes `LANGUAGE`, MP4 `language`).
    fn tag(&self, key: &str) -> Option<&str> {
        self.tags
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.trim().is_empty())
    }

    fn flag(&self, key: &str) -> bool {
        self.disposition.get(key).copied().unwrap_or(0) != 0
    }

    fn language(&self) -> Option<String> {
        self.tag("language").map(|l| l.trim().to_ascii_lowercase())
    }

    /// Track title; MP4 stores it as `handler_name`, which is ignored when
    /// it is only a generic muxer default.
    fn title(&self) -> Option<String> {
        self.tag("title")
            .or_else(|| self.tag("handler_name").filter(|h| !is_generic_handler(h)))
            .map(|t| t.trim().to_string())
    }

    /// Bitrate from the stream or from MKV statistics tags (`BPS`, `BPS-eng`).
    fn bit_rate(&self) -> Option<u64> {
        parse_u64(self.bit_rate.as_deref())
            .or_else(|| parse_u64(self.tag("BPS")))
            .or_else(|| parse_u64(self.tag("BPS-eng")))
    }

    fn side_data(&self, type_contains: &str) -> Option<&Value> {
        self.side_data_list.iter().find(|v| {
            v.get("side_data_type")
                .and_then(Value::as_str)
                .is_some_and(|t| t.contains(type_contains))
        })
    }
}

fn is_generic_handler(name: &str) -> bool {
    let n = name.trim().to_ascii_lowercase();
    n.is_empty()
        || n.contains("handler")
        || n.starts_with("core media")
        || n.starts_with("gpac iso")
        || n.starts_with("mainconcept")
        || n == "sound media handler"
        || n == "video media handler"
}

/// Parse numbers that may be missing or `N/A`.
fn parse_u64(text: Option<&str>) -> Option<u64> {
    text.and_then(|t| t.trim().parse::<u64>().ok())
}

fn parse_f64(text: Option<&str>) -> Option<f64> {
    text.and_then(|t| t.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
}

fn json_u8(v: &Value, key: &str) -> Option<u8> {
    v.get(key)
        .and_then(Value::as_u64)
        .and_then(|n| u8::try_from(n).ok())
}

/// File-system facts collected by the scanner.
#[derive(Debug, Clone)]
pub struct FileFacts {
    pub absolute_path: PathBuf,
    pub relative_path: PathBuf,
    pub size_bytes: u64,
    pub modified_unix: i64,
}

pub fn parse_probe_json(json: &[u8], path: &Path) -> Result<ProbeOutput> {
    serde_json::from_slice(json).map_err(|source| Error::ProbeParse {
        path: path.to_path_buf(),
        source,
    })
}

/// Map ffprobe output to a [`MediaFile`]. Never fails on missing metadata.
pub fn to_media_file(probe: &ProbeOutput, facts: &FileFacts, hdr10_plus: bool) -> MediaFile {
    let mut video = Vec::new();
    let mut audio = Vec::new();
    let mut subtitles = Vec::new();
    let mut other_streams = Vec::new();

    for s in &probe.streams {
        let codec_name = s.codec_name.clone().unwrap_or_else(|| "unknown".into());
        match s.codec_type.as_deref() {
            Some("video") => {
                let type_index = video.len() as u32;
                video.push(map_video(s, codec_name, type_index, hdr10_plus));
            }
            Some("audio") => {
                let codec = classify::audio_codec(&codec_name, s.profile.as_deref());
                let title = s.title();
                audio.push(AudioStream {
                    index: s.index,
                    type_index: audio.len() as u32,
                    atmos: classify::detect_atmos(&codec, s.profile.as_deref(), title.as_deref()),
                    dts_x: classify::detect_dts_x(&codec, s.profile.as_deref(), title.as_deref()),
                    commentary: s.flag("comment")
                        || title.as_deref().is_some_and(|t| {
                            let t = t.to_lowercase();
                            t.contains("comment") || t.contains("kommentar")
                        }),
                    visual_impaired: s.flag("visual_impaired"),
                    codec,
                    codec_name,
                    profile: s.profile.clone(),
                    language: s.language(),
                    title,
                    bit_rate: s.bit_rate(),
                    channels: s.channels,
                    channel_layout: s.channel_layout.clone(),
                    sample_rate: parse_u64(s.sample_rate.as_deref())
                        .and_then(|v| u32::try_from(v).ok()),
                    default: s.flag("default"),
                    forced: s.flag("forced"),
                });
            }
            Some("subtitle") => subtitles.push(SubtitleStream {
                index: s.index,
                type_index: subtitles.len() as u32,
                codec: classify::subtitle_codec(&codec_name),
                codec_name,
                language: s.language(),
                title: s.title(),
                default: s.flag("default"),
                forced: s.flag("forced")
                    || s.title()
                        .is_some_and(|t| t.to_lowercase().contains("forced")),
                hearing_impaired: s.flag("hearing_impaired"),
            }),
            other => other_streams.push(format!(
                "#{} {} ({})",
                s.index,
                other.unwrap_or(tr!("unknown")),
                codec_name
            )),
        }
    }

    let format = probe.format.as_ref();
    let container = ContainerInfo {
        format_name: format
            .and_then(|f| f.format_name.clone())
            .unwrap_or_default(),
        duration_s: format.and_then(|f| parse_f64(f.duration.as_deref())),
        bit_rate: format.and_then(|f| parse_u64(f.bit_rate.as_deref())),
        stream_count: format
            .and_then(|f| f.nb_streams)
            .unwrap_or(probe.streams.len() as u32),
        chapters: probe.chapters.len() as u32,
        title: format.and_then(|f| {
            f.tags
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("title"))
                .map(|(_, v)| v.clone())
        }),
    };

    let file_name = facts
        .absolute_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let extension = facts
        .absolute_path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();

    MediaFile {
        absolute_path: facts.absolute_path.clone(),
        relative_path: facts.relative_path.clone(),
        file_name,
        extension,
        size_bytes: facts.size_bytes,
        modified_unix: facts.modified_unix,
        container,
        video,
        audio,
        subtitles,
        other_streams,
    }
}

fn map_video(
    s: &ProbeStream,
    codec_name: String,
    type_index: u32,
    hdr10_plus: bool,
) -> VideoStream {
    let color = ColorInfo {
        primaries: s.color_primaries.clone().filter(|v| v != "unknown"),
        transfer: s.color_transfer.clone().filter(|v| v != "unknown"),
        space: s.color_space.clone().filter(|v| v != "unknown"),
        range: s.color_range.clone().filter(|v| v != "unknown"),
    };
    let dovi = s.side_data("DOVI configuration").map(|d| {
        let profile = json_u8(d, "dv_profile");
        DolbyVisionInfo {
            profile,
            level: json_u8(d, "dv_level"),
            compatibility: classify::dv_compatibility(
                json_u8(d, "dv_bl_signal_compatibility_id"),
                profile,
            ),
            has_enhancement_layer: json_u8(d, "el_present_flag") == Some(1),
        }
    });
    let signals = HdrSignals {
        dovi,
        codec_tag: s.codec_tag_string.clone(),
        hdr10_plus,
        mastering_display: s.side_data("Mastering display").is_some(),
    };
    let hdr = classify::detect_hdr(&color, &signals);
    let rotation = s
        .side_data("Display Matrix")
        .and_then(|d| d.get("rotation"))
        .and_then(Value::as_i64)
        .and_then(|r| i32::try_from(r).ok())
        .or_else(|| s.tag("rotate").and_then(|r| r.parse().ok()));
    // avg_frame_rate is the real rate for VFR/telecined material; r_frame_rate
    // is the container timebase guess. Prefer avg if plausible.
    let frame_rate = s
        .avg_frame_rate
        .as_deref()
        .and_then(Ratio::parse)
        .filter(|r| r.as_f64() < 400.0)
        .or_else(|| s.r_frame_rate.as_deref().and_then(Ratio::parse));
    let bits_raw = s
        .bits_per_raw_sample
        .as_deref()
        .and_then(|b| b.parse::<u8>().ok());

    VideoStream {
        index: s.index,
        type_index,
        codec: classify::video_codec(&codec_name),
        codec_name,
        profile: s.profile.clone(),
        width: s.width.unwrap_or(0),
        height: s.height.unwrap_or(0),
        sample_aspect_ratio: s.sample_aspect_ratio.as_deref().and_then(Ratio::parse),
        display_aspect_ratio: s.display_aspect_ratio.as_deref().and_then(Ratio::parse),
        frame_rate,
        bit_rate: s.bit_rate(),
        bit_depth: classify::bit_depth(bits_raw, s.pix_fmt.as_deref()),
        pix_fmt: s.pix_fmt.clone(),
        color,
        hdr,
        rotation,
        interlaced: s
            .field_order
            .as_deref()
            .map(|f| !matches!(f, "progressive" | "unknown")),
        default: s.flag("default"),
        attached_pic: s.flag("attached_pic"),
    }
}

/// Run ffprobe for one file.
pub async fn probe(ffprobe: &Path, file: &Path) -> Result<ProbeOutput> {
    let args: Vec<&std::ffi::OsStr> = vec![
        "-v".as_ref(),
        "error".as_ref(),
        "-print_format".as_ref(),
        "json".as_ref(),
        "-show_format".as_ref(),
        "-show_streams".as_ref(),
        "-show_chapters".as_ref(),
        "-i".as_ref(),
        file.as_os_str(),
    ];
    let out = run_capture(
        "ffprobe",
        ffprobe,
        args,
        PROBE_TIMEOUT,
        &trf!("analysis of '{}'", file.display()),
    )
    .await?;
    parse_probe_json(&out.stdout, file)
}

/// Number of packets in one subtitle stream (`s:<type_index>`). Reads the whole
/// file, so it is only used to confirm that an empty export is legitimate.
pub async fn count_subtitle_packets(ffprobe: &Path, file: &Path, type_index: u32) -> Result<u64> {
    let select = format!("s:{type_index}");
    let args: Vec<&std::ffi::OsStr> = vec![
        "-v".as_ref(),
        "error".as_ref(),
        "-count_packets".as_ref(),
        "-select_streams".as_ref(),
        select.as_ref(),
        "-show_entries".as_ref(),
        "stream=nb_read_packets".as_ref(),
        "-of".as_ref(),
        "csv=p=0".as_ref(),
        "-i".as_ref(),
        file.as_os_str(),
    ];
    let out = run_capture(
        "ffprobe",
        ffprobe,
        args,
        std::time::Duration::from_secs(30 * 60),
        &trf!("analysis of '{}'", file.display()),
    )
    .await?;
    Ok(parse_packet_count(&String::from_utf8_lossy(&out.stdout)))
}

/// ffprobe prints `N/A` when no packet of the stream was read.
fn parse_packet_count(text: &str) -> u64 {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .and_then(|l| l.trim_end_matches(',').parse().ok())
        .unwrap_or(0)
}

/// HDR10+ metadata lives in per-frame side data, so a PQ stream needs a short
/// frame probe to tell HDR10 from HDR10+.
pub async fn probe_hdr10_plus(ffprobe: &Path, file: &Path) -> Result<bool> {
    let args: Vec<&std::ffi::OsStr> = vec![
        "-v".as_ref(),
        "error".as_ref(),
        "-select_streams".as_ref(),
        "v:0".as_ref(),
        "-read_intervals".as_ref(),
        "%+#5".as_ref(),
        "-show_frames".as_ref(),
        "-show_entries".as_ref(),
        "frame=side_data_list".as_ref(),
        "-print_format".as_ref(),
        "json".as_ref(),
        "-i".as_ref(),
        file.as_os_str(),
    ];
    let out = run_capture("ffprobe", ffprobe, args, PROBE_TIMEOUT, tr!("HDR10+ check")).await?;
    Ok(frames_have_hdr10_plus(&out.stdout))
}

pub fn frames_have_hdr10_plus(json: &[u8]) -> bool {
    let text = String::from_utf8_lossy(json);
    text.contains("SMPTE2094-40") || text.contains("HDR10+")
}

fn is_unknown(v: Option<&str>) -> bool {
    matches!(v, None | Some("") | Some("unknown") | Some("unspecified"))
}

/// High bit depth video whose stream-level colour description is missing.
fn needs_frame_colors(s: &ProbeStream) -> bool {
    let high_bit_depth = s
        .pix_fmt
        .as_deref()
        .is_some_and(|p| p.contains("10") || p.contains("12"));
    s.codec_type.as_deref() == Some("video")
        && high_bit_depth
        && (is_unknown(s.color_transfer.as_deref()) || is_unknown(s.color_primaries.as_deref()))
}

#[derive(Debug, Default, Deserialize)]
struct FrameColors {
    color_primaries: Option<String>,
    color_transfer: Option<String>,
    color_space: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct FrameColorsOutput {
    #[serde(default)]
    frames: Vec<FrameColors>,
}

fn parse_frame_colors(json: &[u8]) -> FrameColors {
    serde_json::from_slice::<FrameColorsOutput>(json)
        .ok()
        .and_then(|o| o.frames.into_iter().next())
        .unwrap_or_default()
}

/// Only fills in what the stream level does not know.
fn apply_frame_colors(stream: &mut ProbeStream, f: FrameColors) {
    if is_unknown(stream.color_transfer.as_deref()) && !is_unknown(f.color_transfer.as_deref()) {
        stream.color_transfer = f.color_transfer;
    }
    if is_unknown(stream.color_primaries.as_deref()) && !is_unknown(f.color_primaries.as_deref()) {
        stream.color_primaries = f.color_primaries;
    }
    if is_unknown(stream.color_space.as_deref()) && !is_unknown(f.color_space.as_deref()) {
        stream.color_space = f.color_space;
    }
}

async fn probe_first_frame_colors(ffprobe: &Path, file: &Path, index: u32) -> Result<FrameColors> {
    let select = index.to_string();
    let args: Vec<&std::ffi::OsStr> = vec![
        "-v".as_ref(),
        "error".as_ref(),
        "-select_streams".as_ref(),
        select.as_ref(),
        "-read_intervals".as_ref(),
        "%+#1".as_ref(),
        "-show_frames".as_ref(),
        "-show_entries".as_ref(),
        "frame=color_primaries,color_transfer,color_space".as_ref(),
        "-print_format".as_ref(),
        "json".as_ref(),
        "-i".as_ref(),
        file.as_os_str(),
    ];
    let out = run_capture(
        "ffprobe",
        ffprobe,
        args,
        PROBE_TIMEOUT,
        &trf!("analysis of '{}'", file.display()),
    )
    .await?;
    Ok(parse_frame_colors(&out.stdout))
}

/// Probe a file completely (including the HDR10+ refinement for PQ streams).
pub async fn analyze_file(ffprobe: &Path, facts: &FileFacts) -> Result<MediaFile> {
    let mut out = probe(ffprobe, &facts.absolute_path).await?;
    // Some muxer/ffprobe combinations (e.g. ffprobe 8.x on files written by
    // ffmpeg 8.x) only report the colour description on frame level. Without
    // it a real HDR stream would be classified as SDR and not tone mapped.
    for stream in &mut out.streams {
        if needs_frame_colors(stream) {
            match probe_first_frame_colors(ffprobe, &facts.absolute_path, stream.index).await {
                Ok(colors) => apply_frame_colors(stream, colors),
                Err(e) => tracing::warn!("frame colour probe failed: {e}"),
            }
        }
    }
    let is_pq = out.streams.iter().any(|s| {
        s.codec_type.as_deref() == Some("video") && s.color_transfer.as_deref() == Some("smpte2084")
    });
    let hdr10_plus = if is_pq {
        probe_hdr10_plus(ffprobe, &facts.absolute_path)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!("HDR10+ check failed: {e}");
                false
            })
    } else {
        false
    };
    Ok(to_media_file(&out, facts, hdr10_plus))
}

#[cfg(test)]
mod tests {

    #[test]
    fn frame_colors_fill_missing_stream_colors() {
        let json = br#"{"frames":[{"color_space":"bt2020nc","color_primaries":"bt2020","color_transfer":"smpte2084"}]}"#;
        let mut s: ProbeStream = serde_json::from_str(
            r#"{"index":0,"codec_type":"video","pix_fmt":"yuv420p10le","color_space":"bt2020nc","color_transfer":"unknown"}"#,
        )
        .unwrap();
        assert!(needs_frame_colors(&s));
        apply_frame_colors(&mut s, parse_frame_colors(json));
        assert_eq!(s.color_transfer.as_deref(), Some("smpte2084"));
        assert_eq!(s.color_primaries.as_deref(), Some("bt2020"));
        assert!(!needs_frame_colors(&s));
        // 8-bit SDR without tags: no extra probe
        let sdr: ProbeStream =
            serde_json::from_str(r#"{"index":0,"codec_type":"video","pix_fmt":"yuv420p"}"#)
                .unwrap();
        assert!(!needs_frame_colors(&sdr));
    }

    #[test]
    fn packet_count_parsing() {
        assert_eq!(super::parse_packet_count("13\n"), 13);
        assert_eq!(super::parse_packet_count("N/A\n"), 0);
        assert_eq!(super::parse_packet_count(""), 0);
    }

    use super::*;
    use crate::media::model::*;

    fn facts() -> FileFacts {
        FileFacts {
            absolute_path: "/src/Movies/Film.MKV".into(),
            relative_path: "Movies/Film.MKV".into(),
            size_bytes: 10_000_000_000,
            modified_unix: 1_700_000_000,
        }
    }

    const UHD_REMUX: &str = r#"{
      "streams": [
        {"index":0,"codec_name":"hevc","codec_type":"video","profile":"Main 10","codec_tag_string":"[0][0][0][0]",
         "width":3840,"height":2160,"sample_aspect_ratio":"1:1","display_aspect_ratio":"16:9","pix_fmt":"yuv420p10le",
         "color_range":"tv","color_space":"bt2020nc","color_transfer":"smpte2084","color_primaries":"bt2020",
         "field_order":"progressive","r_frame_rate":"24000/1001","avg_frame_rate":"24000/1001",
         "disposition":{"default":1,"forced":0},
         "tags":{"BPS-eng":"58000000","DURATION-eng":"02:00:00.000000000"},
         "side_data_list":[{"side_data_type":"DOVI configuration record","dv_version_major":1,"dv_profile":8,"dv_level":6,
           "rpu_present_flag":1,"el_present_flag":0,"bl_present_flag":1,"dv_bl_signal_compatibility_id":1}]},
        {"index":1,"codec_name":"truehd","codec_type":"audio","sample_rate":"48000","channels":8,"channel_layout":"7.1",
         "disposition":{"default":1,"forced":0},"tags":{"language":"eng","title":"TrueHD Atmos 7.1"}},
        {"index":2,"codec_name":"dts","codec_type":"audio","profile":"DTS-HD MA","sample_rate":"48000","channels":6,
         "channel_layout":"5.1(side)","bit_rate":"N/A","disposition":{"default":0},"tags":{"LANGUAGE":"ger"}},
        {"index":3,"codec_name":"ac3","codec_type":"audio","sample_rate":"48000","channels":2,"bit_rate":"192000",
         "disposition":{"default":0,"comment":1},"tags":{"language":"eng","title":"Commentary"}},
        {"index":4,"codec_name":"hdmv_pgs_subtitle","codec_type":"subtitle","disposition":{"default":0,"forced":1},
         "tags":{"language":"ger","title":"Forced"}},
        {"index":5,"codec_name":"subrip","codec_type":"subtitle","disposition":{"default":0,"forced":0},
         "tags":{"language":"eng"}},
        {"index":6,"codec_name":"ttf","codec_type":"attachment","tags":{"filename":"font.ttf"}},
        {"index":7,"codec_name":"mjpeg","codec_type":"video","width":600,"height":900,"disposition":{"attached_pic":1}}
      ],
      "format": {"format_name":"matroska,webm","duration":"7200.000000","bit_rate":"60000000","nb_streams":8},
      "chapters": [{"id":0},{"id":1}]
    }"#;

    #[test]
    fn parses_uhd_remux() {
        let probe = parse_probe_json(UHD_REMUX.as_bytes(), Path::new("x")).unwrap();
        let m = to_media_file(&probe, &facts(), false);
        assert_eq!(m.extension, "mkv");
        assert_eq!(m.container.duration_s, Some(7200.0));
        assert_eq!(m.container.chapters, 2);
        assert_eq!(m.video.len(), 2);
        let v = m.primary_video().unwrap();
        assert_eq!(v.codec, VideoCodec::Hevc);
        assert_eq!((v.width, v.height), (3840, 2160));
        assert_eq!(v.bit_depth, Some(10));
        assert_eq!(v.bit_rate, Some(58_000_000));
        assert!((v.fps().unwrap() - 23.976).abs() < 0.01);
        match &v.hdr {
            HdrFormat::DolbyVision(dv) => {
                assert_eq!(dv.profile, Some(8));
                assert_eq!(dv.compatibility, DvCompatibility::Hdr10);
            }
            other => panic!("expected DV, got {other:?}"),
        }
        assert!(m.video[1].attached_pic);

        assert_eq!(m.audio.len(), 3);
        assert_eq!(m.audio[0].codec, AudioCodec::TrueHd);
        assert!(matches!(m.audio[0].atmos, Detection::Probable(_)));
        assert_eq!(m.audio[1].codec, AudioCodec::DtsHdMa);
        assert_eq!(m.audio[1].language.as_deref(), Some("ger"));
        assert_eq!(m.audio[1].bit_rate, None);
        assert_eq!(m.audio[1].channel_label(), "5.1");
        assert!(m.audio[2].commentary);
        assert_eq!(m.audio[2].type_index, 2);

        assert_eq!(m.subtitles.len(), 2);
        assert_eq!(m.subtitles[0].codec, SubtitleCodec::Pgs);
        assert_eq!(m.subtitles[0].kind(), SubtitleKind::Bitmap);
        assert!(m.subtitles[0].forced);
        assert_eq!(m.subtitles[1].kind(), SubtitleKind::Text);
        assert_eq!(m.other_streams.len(), 1);
        assert_eq!(m.audio_languages(), vec!["eng", "ger"]);
    }

    #[test]
    fn tolerates_missing_metadata() {
        let json = r#"{"streams":[{"index":0,"codec_type":"video"},{"index":1}],"format":{}}"#;
        let probe = parse_probe_json(json.as_bytes(), Path::new("x")).unwrap();
        let m = to_media_file(&probe, &facts(), false);
        let v = m.primary_video().unwrap();
        assert_eq!(v.codec, VideoCodec::Other("unknown".into()));
        assert_eq!(v.frame_rate, None);
        assert_eq!(v.hdr, HdrFormat::Sdr);
        assert_eq!(m.container.duration_s, None);
        assert_eq!(m.other_streams.len(), 1);
        // empty object
        let probe = parse_probe_json(b"{}", Path::new("x")).unwrap();
        let m = to_media_file(&probe, &facts(), false);
        assert!(m.primary_video().is_none());
    }

    #[test]
    fn handler_name_as_title() {
        let json = r#"{"streams":[
            {"index":0,"codec_type":"audio","codec_name":"aac","tags":{"handler_name":"Deutsch 5.1"}},
            {"index":1,"codec_type":"audio","codec_name":"aac","tags":{"handler_name":"SoundHandler"}},
            {"index":2,"codec_type":"audio","codec_name":"aac","tags":{"title":"T","handler_name":"X"}}]}"#;
        let probe = parse_probe_json(json.as_bytes(), Path::new("x")).unwrap();
        let m = to_media_file(&probe, &facts(), false);
        assert_eq!(m.audio[0].title.as_deref(), Some("Deutsch 5.1"));
        assert_eq!(m.audio[1].title, None);
        assert_eq!(m.audio[2].title.as_deref(), Some("T"));
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_probe_json(b"not json", Path::new("x")).is_err());
    }

    #[test]
    fn hdr10_plus_frames() {
        let json = br#"{"frames":[{"side_data_list":[{"side_data_type":"HDR Dynamic Metadata SMPTE2094-40 (HDR10+)"}]}]}"#;
        assert!(frames_have_hdr10_plus(json));
        assert!(!frames_have_hdr10_plus(br#"{"frames":[{}]}"#));
    }

    #[test]
    fn ratio_parsing() {
        assert_eq!(Ratio::parse("16:9"), Ratio::new(16, 9));
        assert_eq!(Ratio::parse("0/0"), None);
        assert_eq!(Ratio::parse("N/A"), None);
    }
}
