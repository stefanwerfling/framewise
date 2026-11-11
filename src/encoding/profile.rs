//! Encoding profiles (A Universal, B High Quality, C Space Saver, D Custom).

use serde::{Deserialize, Serialize};

use crate::tr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProfileKind {
    Universal,
    HighQuality,
    SpaceSaver,
    Custom,
    /// Chooses codec, CRF and copy-vs-encode per film by measurement.
    Auto,
}

impl ProfileKind {
    pub fn label(self) -> &'static str {
        match self {
            ProfileKind::Universal => tr!("A · Universal compatibility"),
            ProfileKind::HighQuality => tr!("B · High quality"),
            ProfileKind::SpaceSaver => tr!("C · Space saver"),
            ProfileKind::Custom => tr!("D · Custom"),
            ProfileKind::Auto => tr!("E · Automatic"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TargetVideoCodec {
    H264,
    /// Optional – client compatibility is not universal.
    Hevc,
}

impl TargetVideoCodec {
    pub fn label(self) -> &'static str {
        match self {
            TargetVideoCodec::H264 => "H.264 / AVC",
            TargetVideoCodec::Hevc => "HEVC / H.265",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoSettings {
    pub codec: TargetVideoCodec,
    /// Constant rate factor (x264/x265 scale; mapped for hardware encoders).
    pub crf: u8,
    /// x264/x265 preset.
    pub preset: String,
    /// H.264 profile/level for client compatibility.
    pub h264_profile: String,
    pub h264_level: String,
    pub max_width: u32,
    pub max_height: u32,
    /// Copy the video stream if it is already compatible and efficient.
    pub allow_copy: bool,
    /// Bits per pixel above which a compatible stream is still re-encoded.
    pub copy_max_bpp: f64,
    /// Deinterlace frames flagged as interlaced (bwdif).
    pub deinterlace: bool,
    /// Optional x264 tune (film, animation, grain …).
    pub tune: Option<String>,
}

impl Default for VideoSettings {
    fn default() -> Self {
        Self {
            codec: TargetVideoCodec::H264,
            crf: 20,
            preset: "slow".into(),
            h264_profile: "high".into(),
            h264_level: "4.1".into(),
            max_width: 1920,
            max_height: 1080,
            allow_copy: true,
            copy_max_bpp: 0.18,
            deinterlace: true,
            tune: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioStrategy {
    /// AAC stereo compatibility track guaranteed, originals kept where possible.
    Compatibility,
    /// Keep original tracks as far as the container allows; show limitations.
    PreserveOriginal,
}

impl AudioStrategy {
    pub fn label(self) -> &'static str {
        match self {
            AudioStrategy::Compatibility => tr!("Compatibility"),
            AudioStrategy::PreserveOriginal => tr!("Preserve original"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompatStereoMode {
    Never,
    /// One AAC stereo track per audio language (if none exists yet).
    PerLanguage,
    /// Only for the language of the default track.
    PrimaryLanguage,
}

impl CompatStereoMode {
    pub fn label(self) -> &'static str {
        match self {
            CompatStereoMode::Never => tr!("Never"),
            CompatStereoMode::PerLanguage => tr!("Per language"),
            CompatStereoMode::PrimaryLanguage => tr!("Primary language only"),
        }
    }
}

/// Codec for multichannel tracks that cannot be copied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MultichannelCodec {
    /// Dolby Digital – broadest support, max. 5.1.
    Ac3,
    /// Dolby Digital Plus – ffmpeg's encoder supports max. 5.1.
    Eac3,
    /// AAC-LC multichannel – keeps up to 7.1.
    Aac,
}

impl MultichannelCodec {
    pub fn label(self) -> &'static str {
        match self {
            MultichannelCodec::Ac3 => "AC3 (Dolby Digital, ≤ 5.1)",
            MultichannelCodec::Eac3 => "E-AC3 (Dolby Digital Plus, ≤ 5.1)",
            MultichannelCodec::Aac => "AAC-LC (≤ 7.1)",
        }
    }

    pub fn max_channels(self) -> u32 {
        match self {
            MultichannelCodec::Ac3 | MultichannelCodec::Eac3 => 6,
            MultichannelCodec::Aac => 8,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioSettings {
    pub strategy: AudioStrategy,
    pub compat_stereo: CompatStereoMode,
    pub stereo_bitrate_k: u32,
    pub multichannel_codec: MultichannelCodec,
    pub multichannel_bitrate_k: u32,
    /// Mark the AAC stereo track as default instead of the original default.
    pub compat_stereo_default: bool,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            strategy: AudioStrategy::Compatibility,
            compat_stereo: CompatStereoMode::PerLanguage,
            stereo_bitrate_k: 192,
            multichannel_codec: MultichannelCodec::Ac3,
            multichannel_bitrate_k: 640,
            compat_stereo_default: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BitmapSubtitleAction {
    /// Export as sidecar file next to the result (`.sup` / `.mks`).
    Sidecar,
    /// Do not keep – shown as explicit warning before the job starts.
    Skip,
}

impl BitmapSubtitleAction {
    pub fn label(self) -> &'static str {
        match self {
            BitmapSubtitleAction::Sidecar => tr!("Export as sidecar file"),
            BitmapSubtitleAction::Skip => tr!("Do not keep (with warning)"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SubtitleSettings {
    pub bitmap: BitmapSubtitleAction,
    /// Also export text subtitles as `.srt` sidecars (keeps ASS styling out of the way).
    pub text_sidecar: bool,
}

impl Default for SubtitleSettings {
    fn default() -> Self {
        Self {
            bitmap: BitmapSubtitleAction::Sidecar,
            text_sidecar: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HdrStrategy {
    /// Convert to SDR BT.709 with tone mapping.
    TonemapToSdr,
    /// Keep HDR (only with HEVC 10 bit; planned for a later version).
    Preserve,
}

impl HdrStrategy {
    pub fn label(self) -> &'static str {
        match self {
            HdrStrategy::TonemapToSdr => tr!("Tone map to SDR"),
            HdrStrategy::Preserve => tr!("Preserve HDR (HEVC only)"),
        }
    }
}

/// Tone mapping operator of ffmpeg's `tonemap` filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TonemapAlgorithm {
    Hable,
    Mobius,
    Reinhard,
}

impl TonemapAlgorithm {
    pub fn ffmpeg_name(self) -> &'static str {
        match self {
            TonemapAlgorithm::Hable => "hable",
            TonemapAlgorithm::Mobius => "mobius",
            TonemapAlgorithm::Reinhard => "reinhard",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HdrSettings {
    pub strategy: HdrStrategy,
    pub algorithm: TonemapAlgorithm,
    /// Desaturation strength for highlights (0 = keep colours).
    pub desat: f32,
    /// Nominal peak luminance used for linearisation (nits).
    pub npl: u32,
}

impl Default for HdrSettings {
    fn default() -> Self {
        Self {
            strategy: HdrStrategy::TonemapToSdr,
            algorithm: TonemapAlgorithm::Hable,
            desat: 0.0,
            npl: 100,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QualityMetric {
    Off,
    /// SSIM – built into every ffmpeg build.
    Ssim,
    /// VMAF – needs ffmpeg with libvmaf.
    Vmaf,
}

impl QualityMetric {
    pub fn label(self) -> &'static str {
        match self {
            QualityMetric::Off => tr!("Off"),
            QualityMetric::Ssim => "SSIM",
            QualityMetric::Vmaf => "VMAF",
        }
    }

    /// Typical score drop per CRF step (x264/x265 at 1080p), used by the CRF
    /// search until it has measured a slope of its own.
    pub fn typical_slope(self) -> f64 {
        match self {
            QualityMetric::Vmaf => 1.0,
            QualityMetric::Off | QualityMetric::Ssim => 0.002,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct QualitySettings {
    pub metric: QualityMetric,
    pub min_vmaf: f64,
    pub min_ssim: f64,
    /// Search the highest CRF that still meets the minimum quality.
    pub smart_crf: bool,
    pub crf_min: u8,
    pub crf_max: u8,
    pub sample_count: u32,
    pub sample_seconds: u32,
    /// Fail the job if the measured quality is below the minimum.
    pub mandatory: bool,
    /// Pick sample windows by source complexity (hardest scenes plus typical
    /// ones) instead of evenly spaced.
    pub scene_windows: bool,
    /// The minimum applies to the weakest sample, not to the average.
    pub judge_min: bool,
    /// Also search an HEVC encode and use it when it is clearly smaller.
    /// H.264 stays the default and the fallback.
    pub compare_hevc: bool,
    /// Size advantage HEVC needs over H.264 (0.15 = 15 % smaller).
    pub hevc_min_saving: f64,
    /// x265 preset for the HEVC candidate.
    pub hevc_preset: String,
    /// Below this saving the video stream is copied instead (if possible).
    pub min_saving: f64,
    /// Time budget for the analysis before encoding (minutes).
    pub analysis_budget_min: u32,
}

impl Default for QualitySettings {
    fn default() -> Self {
        Self {
            metric: QualityMetric::Off,
            min_vmaf: 93.0,
            min_ssim: 0.985,
            smart_crf: false,
            crf_min: 18,
            crf_max: 26,
            sample_count: 3,
            sample_seconds: 20,
            mandatory: false,
            scene_windows: false,
            judge_min: false,
            compare_hevc: false,
            hevc_min_saving: 0.15,
            hevc_preset: "medium".into(),
            min_saving: 0.0,
            analysis_budget_min: 30,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EncodingProfile {
    pub id: String,
    pub name: String,
    pub kind: ProfileKind,
    pub builtin: bool,
    #[serde(default)]
    pub video: VideoSettings,
    #[serde(default)]
    pub audio: AudioSettings,
    #[serde(default)]
    pub subtitles: SubtitleSettings,
    #[serde(default)]
    pub hdr: HdrSettings,
    #[serde(default)]
    pub quality: QualitySettings,
}

impl EncodingProfile {
    /// Profile A – default.
    pub fn universal() -> Self {
        Self {
            id: "universal".into(),
            name: "Universal Compatibility".into(),
            kind: ProfileKind::Universal,
            builtin: true,
            video: VideoSettings::default(),
            audio: AudioSettings::default(),
            subtitles: SubtitleSettings::default(),
            hdr: HdrSettings::default(),
            quality: QualitySettings::default(),
        }
    }

    /// Profile B.
    pub fn high_quality() -> Self {
        Self {
            id: "high_quality".into(),
            name: "High Quality".into(),
            kind: ProfileKind::HighQuality,
            video: VideoSettings {
                crf: 18,
                preset: "slower".into(),
                copy_max_bpp: 0.25,
                ..VideoSettings::default()
            },
            audio: AudioSettings {
                strategy: AudioStrategy::PreserveOriginal,
                multichannel_codec: MultichannelCodec::Eac3,
                multichannel_bitrate_k: 768,
                stereo_bitrate_k: 256,
                ..AudioSettings::default()
            },
            ..Self::universal()
        }
    }

    /// Profile C – quality check is mandatory.
    pub fn space_saver() -> Self {
        Self {
            id: "space_saver".into(),
            name: "Space Saver".into(),
            kind: ProfileKind::SpaceSaver,
            video: VideoSettings {
                crf: 23,
                allow_copy: false,
                ..VideoSettings::default()
            },
            audio: AudioSettings {
                compat_stereo: CompatStereoMode::PrimaryLanguage,
                stereo_bitrate_k: 160,
                multichannel_bitrate_k: 448,
                ..AudioSettings::default()
            },
            quality: QualitySettings {
                metric: QualityMetric::Ssim,
                mandatory: true,
                smart_crf: true,
                crf_min: 20,
                crf_max: 28,
                ..QualitySettings::default()
            },
            ..Self::universal()
        }
    }

    /// Profile D – starting point for user profiles.
    pub fn custom(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            kind: ProfileKind::Custom,
            builtin: false,
            ..Self::universal()
        }
    }

    /// Profile E – the method is chosen per film: representative scenes,
    /// quality floor on the weakest one, H.264 vs. HEVC, copy if not worth it.
    pub fn auto() -> Self {
        Self {
            id: "auto".into(),
            name: "Automatic (best method)".into(),
            kind: ProfileKind::Auto,
            quality: QualitySettings {
                metric: QualityMetric::Vmaf,
                min_vmaf: 95.0,
                min_ssim: 0.99,
                smart_crf: true,
                crf_min: 16,
                crf_max: 28,
                sample_count: 8,
                sample_seconds: 10,
                mandatory: false,
                scene_windows: true,
                judge_min: true,
                compare_hevc: true,
                hevc_min_saving: 0.15,
                hevc_preset: "medium".into(),
                min_saving: 0.10,
                analysis_budget_min: 30,
            },
            ..Self::universal()
        }
    }

    pub fn builtins() -> Vec<Self> {
        vec![
            Self::universal(),
            Self::high_quality(),
            Self::space_saver(),
            Self::auto(),
        ]
    }

    /// Profile C requires a quality check.
    pub fn quality_check_required(&self) -> bool {
        self.quality.mandatory || self.kind == ProfileKind::SpaceSaver
    }
}
