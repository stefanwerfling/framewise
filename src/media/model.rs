//! Domain model for media files and their streams.
//!
//! All technical metadata is optional: real-world files frequently lack tags,
//! bitrates or color information, and the model must represent that honestly
//! instead of guessing.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::analyzer::resolution::CropRect;

use crate::{tr, trf};

/// Rational number as used by ffprobe (`30000/1001`, `16:9`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ratio {
    pub num: u32,
    pub den: u32,
}

impl Ratio {
    pub fn new(num: u32, den: u32) -> Option<Self> {
        (num > 0 && den > 0).then_some(Self { num, den })
    }

    /// Parse `a/b` or `a:b`. Returns `None` for `0/0`, `N/A` etc.
    pub fn parse(text: &str) -> Option<Self> {
        let (a, b) = text.split_once(['/', ':'])?;
        Self::new(a.trim().parse().ok()?, b.trim().parse().ok()?)
    }

    pub fn as_f64(self) -> f64 {
        f64::from(self.num) / f64::from(self.den)
    }
}

impl fmt::Display for Ratio {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.num, self.den)
    }
}

/// Where a detection came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DetectionSource {
    /// Codec profile reported by ffprobe (value kept verbatim).
    CodecProfile(String),
    /// Keyword in the track title.
    TrackTitle,
}

/// Certainty of a feature detection (Atmos, DTS:X …).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Detection {
    #[default]
    No,
    /// Heuristic hint, e.g. from a track title.
    Probable(DetectionSource),
    /// Reported by the codec profile / stream metadata.
    Confirmed(DetectionSource),
}

impl Detection {
    pub fn is_present(&self) -> bool {
        !matches!(self, Detection::No)
    }

    /// Localised description, e.g. "wahrscheinlich (Track-Titel)".
    pub fn describe(&self) -> String {
        let source = |s: &DetectionSource| match s {
            DetectionSource::CodecProfile(p) => trf!("codec profile \"{}\"", p),
            DetectionSource::TrackTitle => tr!("track title").to_string(),
        };
        match self {
            Detection::No => tr!("no").into(),
            Detection::Probable(s) => trf!("probable ({})", source(s)),
            Detection::Confirmed(s) => trf!("yes ({})", source(s)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VideoCodec {
    H264,
    Hevc,
    Av1,
    Vp9,
    Vp8,
    Mpeg2,
    Mpeg4,
    Vc1,
    Other(String),
}

impl VideoCodec {
    pub fn label(&self) -> &str {
        match self {
            VideoCodec::H264 => "H.264",
            VideoCodec::Hevc => "HEVC",
            VideoCodec::Av1 => "AV1",
            VideoCodec::Vp9 => "VP9",
            VideoCodec::Vp8 => "VP8",
            VideoCodec::Mpeg2 => "MPEG-2",
            VideoCodec::Mpeg4 => "MPEG-4 ASP",
            VideoCodec::Vc1 => "VC-1",
            VideoCodec::Other(name) => name,
        }
    }

    /// Rough efficiency relative to H.264 (used for size estimates only).
    pub fn efficiency_vs_h264(&self) -> f64 {
        match self {
            VideoCodec::H264 => 1.0,
            VideoCodec::Hevc | VideoCodec::Vp9 => 1.5,
            VideoCodec::Av1 => 1.8,
            VideoCodec::Mpeg2 => 0.5,
            VideoCodec::Mpeg4 | VideoCodec::Vc1 | VideoCodec::Vp8 => 0.7,
            VideoCodec::Other(_) => 0.7,
        }
    }
}

/// Mastering display colour volume (SMPTE ST 2086) in x265 units:
/// chromaticity in 0.00002, luminance in 0.0001 cd/m².
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MasteringDisplay {
    pub red: (u32, u32),
    pub green: (u32, u32),
    pub blue: (u32, u32),
    pub white_point: (u32, u32),
    pub max_luminance: u32,
    pub min_luminance: u32,
}

impl MasteringDisplay {
    /// `G(x,y)B(x,y)R(x,y)WP(x,y)L(max,min)` as x265's `master-display`.
    pub fn x265(&self) -> String {
        let p = |(x, y): (u32, u32)| format!("({x},{y})");
        format!(
            "G{}B{}R{}WP{}L({},{})",
            p(self.green),
            p(self.blue),
            p(self.red),
            p(self.white_point),
            self.max_luminance,
            self.min_luminance
        )
    }
}

/// Static HDR10 metadata of a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HdrMetadata {
    pub mastering: Option<MasteringDisplay>,
    /// Maximum content / frame-average light level in cd/m² (CTA-861.3).
    pub max_cll: Option<(u32, u32)>,
}

/// Dynamic range of a video stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum HdrFormat {
    #[default]
    Sdr,
    Hdr10,
    Hdr10Plus,
    Hlg,
    DolbyVision(DolbyVisionInfo),
    /// PQ/HLG transfer could not be determined reliably.
    Unknown,
}

impl HdrFormat {
    pub fn is_hdr(&self) -> bool {
        !matches!(self, HdrFormat::Sdr)
    }

    pub fn label(&self) -> String {
        match self {
            HdrFormat::Sdr => "SDR".into(),
            HdrFormat::Hdr10 => "HDR10".into(),
            HdrFormat::Hdr10Plus => "HDR10+".into(),
            HdrFormat::Hlg => "HLG".into(),
            HdrFormat::DolbyVision(dv) => match dv.profile {
                Some(p) => {
                    let base = match dv.compatibility {
                        DvCompatibility::Hdr10 => tr!(" (HDR10 base)"),
                        DvCompatibility::Sdr => tr!(" (SDR base)"),
                        DvCompatibility::Hlg => tr!(" (HLG base)"),
                        DvCompatibility::None => tr!(" (no base layer)"),
                        DvCompatibility::Unknown => "",
                    };
                    format!("Dolby Vision P{p}{base}")
                }
                None => "Dolby Vision".into(),
            },
            HdrFormat::Unknown => "HDR?".into(),
        }
    }
}

/// Base layer compatibility of a Dolby Vision stream
/// (`dv_bl_signal_compatibility_id`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DvCompatibility {
    /// Profile 5 – IPTPQc2, no usable fallback without DV processing.
    None,
    Hdr10,
    Sdr,
    Hlg,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DolbyVisionInfo {
    pub profile: Option<u8>,
    pub level: Option<u8>,
    pub compatibility: DvCompatibility,
    pub has_enhancement_layer: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ColorInfo {
    pub primaries: Option<String>,
    pub transfer: Option<String>,
    pub space: Option<String>,
    pub range: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoStream {
    /// Absolute stream index in the container.
    pub index: u32,
    /// Index among video streams (`0:v:N`).
    pub type_index: u32,
    pub codec: VideoCodec,
    pub codec_name: String,
    pub profile: Option<String>,
    pub width: u32,
    pub height: u32,
    pub sample_aspect_ratio: Option<Ratio>,
    pub display_aspect_ratio: Option<Ratio>,
    pub frame_rate: Option<Ratio>,
    pub bit_rate: Option<u64>,
    pub pix_fmt: Option<String>,
    pub bit_depth: Option<u8>,
    pub color: ColorInfo,
    pub hdr: HdrFormat,
    /// Rotation in degrees from the display matrix.
    pub rotation: Option<i32>,
    pub interlaced: Option<bool>,
    pub default: bool,
    /// Cover art etc. – not a real video track.
    pub attached_pic: bool,
    /// Picture area without black bars. Not part of the scan: detected right
    /// before encoding (see `encoding::crop`).
    #[serde(default)]
    pub crop: Option<CropRect>,
    /// Static HDR10 metadata. Not part of the scan: read right before an
    /// encode that keeps HDR (`None` = not read).
    #[serde(default)]
    pub hdr_metadata: Option<HdrMetadata>,
}

impl VideoStream {
    pub fn fps(&self) -> Option<f64> {
        self.frame_rate.map(Ratio::as_f64)
    }

    /// Chroma subsampling 4:2:0 according to the pixel format.
    pub fn is_yuv420(&self) -> Option<bool> {
        self.pix_fmt.as_deref().map(|p| {
            p.starts_with("yuv420") || p.starts_with("yuvj420") || p == "nv12" || p == "p010le"
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioCodec {
    Aac,
    Ac3,
    Eac3,
    Dts,
    DtsHdMa,
    DtsHdHra,
    TrueHd,
    Flac,
    Opus,
    Vorbis,
    Mp3,
    Alac,
    Pcm,
    Other(String),
}

impl AudioCodec {
    pub fn label(&self) -> &str {
        match self {
            AudioCodec::Aac => "AAC",
            AudioCodec::Ac3 => "Dolby Digital (AC3)",
            AudioCodec::Eac3 => "Dolby Digital Plus (E-AC3)",
            AudioCodec::Dts => "DTS",
            AudioCodec::DtsHdMa => "DTS-HD MA",
            AudioCodec::DtsHdHra => "DTS-HD HRA",
            AudioCodec::TrueHd => "Dolby TrueHD",
            AudioCodec::Flac => "FLAC",
            AudioCodec::Opus => "Opus",
            AudioCodec::Vorbis => "Vorbis",
            AudioCodec::Mp3 => "MP3",
            AudioCodec::Alac => "ALAC",
            AudioCodec::Pcm => "PCM",
            AudioCodec::Other(name) => name,
        }
    }

    pub fn is_dts_family(&self) -> bool {
        matches!(
            self,
            AudioCodec::Dts | AudioCodec::DtsHdMa | AudioCodec::DtsHdHra
        )
    }

    pub fn is_lossless(&self) -> bool {
        matches!(
            self,
            AudioCodec::DtsHdMa
                | AudioCodec::TrueHd
                | AudioCodec::Flac
                | AudioCodec::Alac
                | AudioCodec::Pcm
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioStream {
    pub index: u32,
    pub type_index: u32,
    pub codec: AudioCodec,
    pub codec_name: String,
    pub profile: Option<String>,
    pub language: Option<String>,
    pub title: Option<String>,
    pub bit_rate: Option<u64>,
    pub channels: Option<u32>,
    pub channel_layout: Option<String>,
    pub sample_rate: Option<u32>,
    pub default: bool,
    pub forced: bool,
    pub atmos: Detection,
    pub dts_x: Detection,
    pub commentary: bool,
    pub visual_impaired: bool,
}

impl AudioStream {
    pub fn channels_or_default(&self) -> u32 {
        self.channels.unwrap_or(2)
    }

    /// Human-readable channel description, e.g. `5.1`.
    pub fn channel_label(&self) -> String {
        match (&self.channel_layout, self.channels) {
            (Some(layout), _) => layout.split('(').next().unwrap_or(layout).to_string(),
            (None, Some(n)) => format!("{n} ch"),
            (None, None) => "?".into(),
        }
    }

    pub fn is_aac_lc(&self) -> bool {
        self.codec == AudioCodec::Aac
            && self
                .profile
                .as_deref()
                .is_none_or(|p| p.eq_ignore_ascii_case("LC"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubtitleCodec {
    Subrip,
    Ass,
    Ssa,
    WebVtt,
    MovText,
    Pgs,
    VobSub,
    DvbSub,
    Other(String),
}

impl SubtitleCodec {
    pub fn label(&self) -> &str {
        match self {
            SubtitleCodec::Subrip => "SRT",
            SubtitleCodec::Ass => "ASS",
            SubtitleCodec::Ssa => "SSA",
            SubtitleCodec::WebVtt => "WebVTT",
            SubtitleCodec::MovText => "mov_text",
            SubtitleCodec::Pgs => "PGS",
            SubtitleCodec::VobSub => "VobSub",
            SubtitleCodec::DvbSub => "DVB",
            SubtitleCodec::Other(name) => name,
        }
    }

    pub fn kind(&self) -> SubtitleKind {
        match self {
            SubtitleCodec::Subrip
            | SubtitleCodec::Ass
            | SubtitleCodec::Ssa
            | SubtitleCodec::WebVtt
            | SubtitleCodec::MovText => SubtitleKind::Text,
            SubtitleCodec::Pgs | SubtitleCodec::VobSub | SubtitleCodec::DvbSub => {
                SubtitleKind::Bitmap
            }
            SubtitleCodec::Other(_) => SubtitleKind::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubtitleKind {
    Text,
    Bitmap,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubtitleStream {
    pub index: u32,
    pub type_index: u32,
    pub codec: SubtitleCodec,
    pub codec_name: String,
    pub language: Option<String>,
    pub title: Option<String>,
    pub default: bool,
    pub forced: bool,
    pub hearing_impaired: bool,
}

impl SubtitleStream {
    pub fn kind(&self) -> SubtitleKind {
        self.codec.kind()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ContainerInfo {
    pub format_name: String,
    pub duration_s: Option<f64>,
    pub bit_rate: Option<u64>,
    pub stream_count: u32,
    pub chapters: u32,
    pub title: Option<String>,
}

/// Fully analysed media file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaFile {
    pub absolute_path: PathBuf,
    /// Path relative to the scanned source root.
    pub relative_path: PathBuf,
    pub file_name: String,
    pub extension: String,
    pub size_bytes: u64,
    pub modified_unix: i64,
    pub container: ContainerInfo,
    pub video: Vec<VideoStream>,
    pub audio: Vec<AudioStream>,
    pub subtitles: Vec<SubtitleStream>,
    /// Streams ffprobe reported but we do not handle (data, attachments).
    pub other_streams: Vec<String>,
}

impl MediaFile {
    /// The main video stream (first non-cover-art stream).
    pub fn primary_video(&self) -> Option<&VideoStream> {
        self.video.iter().find(|v| !v.attached_pic)
    }

    pub fn duration_s(&self) -> Option<f64> {
        self.container.duration_s
    }

    /// Overall bitrate; computed from size and duration if not reported.
    pub fn overall_bitrate(&self) -> Option<u64> {
        self.container.bit_rate.or_else(|| {
            self.duration_s()
                .filter(|d| *d > 0.0)
                .map(|d| (self.size_bytes as f64 * 8.0 / d) as u64)
        })
    }

    /// Video bitrate; estimated from the overall bitrate minus audio if the
    /// stream does not report it (common for MKV).
    pub fn video_bitrate(&self) -> Option<u64> {
        let v = self.primary_video()?;
        v.bit_rate.or_else(|| {
            let total = self.overall_bitrate()?;
            let audio: u64 = self.audio.iter().map(|a| a.bit_rate.unwrap_or(0)).sum();
            Some(total.saturating_sub(audio))
        })
    }

    /// Folder relative to the source root.
    pub fn folder(&self) -> PathBuf {
        self.relative_path
            .parent()
            .map(PathBuf::from)
            .unwrap_or_default()
    }

    pub fn audio_languages(&self) -> Vec<String> {
        let mut langs: Vec<String> = Vec::new();
        for a in &self.audio {
            let l = a.language.clone().unwrap_or_else(|| "und".into());
            if !langs.contains(&l) {
                langs.push(l);
            }
        }
        langs
    }
}
