//! Jellyfin items → [`MediaFile`].
//!
//! Jellyfin determines its stream information with ffprobe as well, so codec
//! names, profiles and colour tags are the same strings framewise gets from
//! ffprobe and go through the same classification. What Jellyfin does not
//! provide (frame side data, chapters, container format name) stays empty.

use std::path::{Path, PathBuf};

use super::dto::{BaseItem, MediaSource, MediaStream};
use crate::media::classify::{self, HdrSignals};
use crate::media::model::{
    AudioStream, ColorInfo, ContainerInfo, DolbyVisionInfo, MediaFile, Ratio, SubtitleStream,
    VideoStream,
};
use crate::provider::{ItemKind, ProviderInfo, ProviderKind};

/// The library an item was found in.
#[derive(Debug, Clone, Default)]
pub struct LibraryContext {
    pub name: String,
    /// Folders of the library on the server (to derive relative paths).
    pub locations: Vec<String>,
}

/// One [`MediaFile`] per media source (version) of a movie or episode.
/// Virtual items (missing episodes) and items without a file are skipped.
pub fn map_item(item: &BaseItem, library: &LibraryContext) -> Vec<MediaFile> {
    let kind = match item.item_type.as_deref() {
        Some("Movie") => ItemKind::Movie,
        Some("Episode") => ItemKind::Episode,
        _ => return Vec::new(),
    };
    if item.location_type.as_deref() == Some("Virtual") {
        return Vec::new();
    }
    let item_id = item.id.clone().unwrap_or_default();
    let server_id = item.server_id.clone().unwrap_or_default();
    let modified_unix = item
        .date_created
        .as_deref()
        .and_then(parse_iso_date)
        .unwrap_or(0);

    item.media_sources
        .iter()
        .filter_map(|source| {
            let server_path = source.path.clone().or_else(|| item.path.clone())?;
            let info = ProviderInfo {
                provider: ProviderKind::Jellyfin,
                server_id: server_id.clone(),
                item_id: item_id.clone(),
                media_source_id: source.id.clone().unwrap_or_else(|| item_id.clone()),
                kind,
                title: item.name.clone().unwrap_or_default(),
                year: item.production_year,
                series: item.series_name.clone(),
                season: item.parent_index_number,
                episode: item.index_number,
                library: library.name.clone(),
                server_path,
                missing_streams: !source
                    .media_streams
                    .iter()
                    .any(|s| s.stream_type.as_deref() == Some("Video")),
            };
            Some(map_source(source, info, library, modified_unix))
        })
        .collect()
}

fn map_source(
    source: &MediaSource,
    info: ProviderInfo,
    library: &LibraryContext,
    modified_unix: i64,
) -> MediaFile {
    let mut video = Vec::new();
    let mut audio = Vec::new();
    let mut subtitles = Vec::new();
    let mut other_streams = Vec::new();

    // External subtitle files are not part of the container.
    for s in source.media_streams.iter().filter(|s| !s.is_external) {
        let index = s.index.unwrap_or(0);
        let codec_name = s.codec.as_deref().unwrap_or("unknown").to_ascii_lowercase();
        match s.stream_type.as_deref() {
            Some("Video") => {
                let type_index = video.len() as u32;
                video.push(map_video(s, codec_name, type_index, false));
            }
            Some("EmbeddedImage") => {
                let type_index = video.len() as u32;
                video.push(map_video(s, codec_name, type_index, true));
            }
            Some("Audio") => {
                let codec = classify::audio_codec(&codec_name, s.profile.as_deref());
                let title = s.title.clone();
                audio.push(AudioStream {
                    index,
                    type_index: audio.len() as u32,
                    atmos: classify::detect_atmos(&codec, s.profile.as_deref(), title.as_deref()),
                    dts_x: classify::detect_dts_x(&codec, s.profile.as_deref(), title.as_deref()),
                    commentary: title.as_deref().is_some_and(|t| {
                        let t = t.to_lowercase();
                        t.contains("comment") || t.contains("kommentar")
                    }),
                    visual_impaired: false,
                    codec,
                    codec_name,
                    profile: s.profile.clone(),
                    language: language(s),
                    title,
                    bit_rate: s.bit_rate,
                    channels: s.channels,
                    channel_layout: s.channel_layout.clone(),
                    sample_rate: s.sample_rate,
                    default: s.is_default,
                    forced: s.is_forced,
                });
            }
            Some("Subtitle") => subtitles.push(SubtitleStream {
                index,
                type_index: subtitles.len() as u32,
                codec: classify::subtitle_codec(&codec_name),
                codec_name,
                language: language(s),
                title: s.title.clone(),
                default: s.is_default,
                forced: s.is_forced
                    || s.title
                        .as_deref()
                        .is_some_and(|t| t.to_lowercase().contains("forced")),
                hearing_impaired: s.is_hearing_impaired,
            }),
            other => other_streams.push(format!(
                "#{index} {} ({codec_name})",
                other.unwrap_or("?").to_ascii_lowercase()
            )),
        }
    }

    let path = Path::new(&info.server_path);
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let extension = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let stream_count = (video.len() + audio.len() + subtitles.len() + other_streams.len()) as u32;

    MediaFile {
        // Not a local path: a stable key that cannot collide with local files.
        absolute_path: PathBuf::from(format!(
            "jellyfin://{}/{}",
            info.server_id, info.media_source_id
        )),
        relative_path: relative_path(&info, library),
        file_name,
        extension,
        size_bytes: source.size.unwrap_or(0),
        modified_unix,
        container: ContainerInfo {
            format_name: source.container.clone().unwrap_or_default(),
            duration_s: source.run_time_ticks.map(|t| t as f64 / 10_000_000.0),
            bit_rate: source.bitrate,
            stream_count,
            chapters: 0,
            title: None,
        },
        video,
        audio,
        subtitles,
        other_streams,
        provider: Some(info),
    }
}

fn map_video(
    s: &MediaStream,
    codec_name: String,
    type_index: u32,
    attached_pic: bool,
) -> VideoStream {
    let range = s.video_range_type.as_deref().unwrap_or_default();
    let mut color = ColorInfo {
        primaries: s.color_primaries.clone().filter(|v| v != "unknown"),
        transfer: s.color_transfer.clone().filter(|v| v != "unknown"),
        space: s.color_space.clone().filter(|v| v != "unknown"),
        range: s.color_range.clone().filter(|v| v != "unknown"),
    };
    // Older servers leave the transfer out but report the range type.
    if color.transfer.is_none() {
        color.transfer = match range {
            "HDR10" | "HDR10Plus" | "DOVIWithHDR10" | "DOVIWithHDR10Plus" => {
                Some("smpte2084".into())
            }
            "HLG" | "DOVIWithHLG" => Some("arib-std-b67".into()),
            _ => None,
        };
    }
    let dovi = (s.dv_profile.is_some() || range.starts_with("DOVI")).then(|| DolbyVisionInfo {
        profile: s.dv_profile,
        level: s.dv_level,
        compatibility: classify::dv_compatibility(s.dv_bl_signal_compatibility_id, s.dv_profile),
        has_enhancement_layer: s.el_present_flag == Some(1) || range.contains("WithEL"),
    });
    let signals = HdrSignals {
        dovi,
        codec_tag: s.codec_tag.clone(),
        hdr10_plus: range.contains("HDR10Plus") || s.hdr10_plus_present_flag == Some(true),
        mastering_display: false,
    };
    let hdr = classify::detect_hdr(&color, &signals);

    VideoStream {
        index: s.index.unwrap_or(0),
        type_index,
        codec: classify::video_codec(&codec_name),
        codec_name,
        profile: s.profile.clone(),
        width: s.width.unwrap_or(0),
        height: s.height.unwrap_or(0),
        sample_aspect_ratio: None,
        display_aspect_ratio: s.aspect_ratio.as_deref().and_then(Ratio::parse),
        frame_rate: s
            .real_frame_rate
            .or(s.average_frame_rate)
            .and_then(frame_rate),
        bit_rate: s.bit_rate,
        bit_depth: classify::bit_depth(s.bit_depth, s.pixel_format.as_deref()),
        pix_fmt: s.pixel_format.clone(),
        color,
        hdr,
        rotation: None,
        interlaced: s.is_interlaced,
        default: s.is_default,
        attached_pic,
        crop: None,
        hdr_metadata: None,
        variable_frame_rate: crate::media::model::is_variable_frame_rate(
            s.real_frame_rate,
            s.average_frame_rate,
        ),
    }
}

fn language(s: &MediaStream) -> Option<String> {
    s.language.clone().filter(|l| !l.is_empty() && l != "und")
}

/// `23.976…` → `24000/1001`, `25` → `25/1`.
pub fn frame_rate(fps: f64) -> Option<Ratio> {
    if !(fps.is_finite() && fps > 0.0 && fps < 400.0) {
        return None;
    }
    let ntsc = (fps * 1.001).round();
    if (fps - fps.round()).abs() > 0.001 && (ntsc * 1000.0 / 1001.0 - fps).abs() < 0.01 {
        return Ratio::new(ntsc as u32 * 1000, 1001);
    }
    Ratio::new((fps * 1000.0).round() as u32, 1000).map(|r| {
        let g = gcd(r.num, r.den);
        Ratio {
            num: r.num / g,
            den: r.den / g,
        }
    })
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// Path below the library folder (like the folder structure of a local
/// source), otherwise `Series/Season N/file` or `Library/file`.
fn relative_path(info: &ProviderInfo, library: &LibraryContext) -> PathBuf {
    let path = Path::new(&info.server_path);
    for loc in &library.locations {
        if let Ok(rel) = path.strip_prefix(loc)
            && !rel.as_os_str().is_empty()
        {
            return rel.to_path_buf();
        }
    }
    let file = path.file_name().map(PathBuf::from).unwrap_or_default();
    match (&info.series, info.season) {
        (Some(series), Some(season)) => PathBuf::from(series)
            .join(format!("Season {season}"))
            .join(file),
        (Some(series), None) => PathBuf::from(series).join(file),
        _ => PathBuf::from(&library.name).join(file),
    }
}

/// `2024-03-01T12:34:56.0000000Z` → Unix seconds (UTC, date and time only).
pub(super) fn parse_iso_date(text: &str) -> Option<i64> {
    let (date, time) = text.split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let mut t = time
        .split([':', '.', 'Z', '+'])
        .map(|p| p.parse::<i64>().ok());
    let (h, min, sec) = (t.next()??, t.next()??, t.next()??);
    // Days from civil (Howard Hinnant).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + min * 60 + sec)
}
