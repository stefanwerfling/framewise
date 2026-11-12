//! Encode planning: decides for every source stream what happens to it.
//!
//! The plan is the single source of truth for the GUI (warnings before the
//! job starts), the ffmpeg command builder, the validation (expected output
//! streams) and the analyzer (size estimate). No stream disappears without an
//! entry in [`EncodePlan`].

use serde::{Deserialize, Serialize};

use super::profile::{
    AudioStrategy, BitmapSubtitleAction, CompatStereoMode, EncodingProfile, HdrStrategy,
    MultichannelCodec, TargetVideoCodec,
};
use crate::analyzer::resolution::{self, Dimensions, ResizeOp, TargetResolution};
use crate::media::model::{
    AudioCodec, AudioStream, DvCompatibility, HdrFormat, MediaFile, SubtitleCodec, SubtitleKind,
    SubtitleStream, VideoCodec, VideoStream,
};
use crate::{tr, trf};

/// Tool capabilities relevant for planning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanCapabilities {
    pub zscale: bool,
    pub tonemap: bool,
    pub libx264: bool,
    pub libx265: bool,
}

impl Default for PlanCapabilities {
    /// Optimistic default used before the hardware scan finished.
    fn default() -> Self {
        Self {
            zscale: true,
            tonemap: true,
            libx264: true,
            libx265: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HdrConversion {
    None,
    /// Tone map PQ/HLG to SDR BT.709.
    TonemapToSdr {
        source: HdrFormat,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum VideoAction {
    Copy,
    Encode {
        codec: TargetVideoCodec,
        crf: u8,
        resolution: TargetResolution,
        hdr: HdrConversion,
        deinterlace: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoPlan {
    pub source_type_index: u32,
    /// Displayed size (after rotation metadata).
    pub source: Dimensions,
    /// Stored (coded) size – what a stream copy keeps.
    pub storage: Dimensions,
    pub action: VideoAction,
    /// Why the stream is (re-)encoded – empty for copies.
    pub reasons: Vec<VideoReason>,
}

impl VideoPlan {
    pub fn output_dimensions(&self) -> Dimensions {
        match &self.action {
            VideoAction::Copy => self.storage,
            VideoAction::Encode { resolution, .. } => resolution.output,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum VideoReason {
    Codec(String),
    BitDepth(u8),
    PixelFormat(String),
    Hdr(String),
    Resolution(Dimensions),
    OddDimensions,
    Interlaced,
    HighBitrate { bpp: f64 },
    ProfileForcesEncode,
}

impl VideoReason {
    pub fn describe(&self) -> String {
        match self {
            VideoReason::Codec(c) => trf!("codec {} is not the target format", c),
            VideoReason::BitDepth(b) => trf!("{} bit instead of 8 bit", b),
            VideoReason::PixelFormat(p) => trf!("pixel format {} instead of yuv420p", p),
            VideoReason::Hdr(h) => trf!("{} → SDR", h),
            VideoReason::Resolution(d) => trf!("resolution {} above target", d),
            VideoReason::OddDimensions => tr!("odd dimensions").into(),
            VideoReason::Interlaced => tr!("interlaced source").into(),
            VideoReason::HighBitrate { bpp } => {
                trf!("high bitrate ({} bits/pixel)", format!("{bpp:.2}"))
            }
            VideoReason::ProfileForcesEncode => tr!("profile requires re-encoding").into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputAudioCodec {
    Aac,
    Ac3,
    Eac3,
}

impl OutputAudioCodec {
    pub fn ffmpeg_encoder(self) -> &'static str {
        match self {
            OutputAudioCodec::Aac => "aac",
            OutputAudioCodec::Ac3 => "ac3",
            OutputAudioCodec::Eac3 => "eac3",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            OutputAudioCodec::Aac => "AAC",
            OutputAudioCodec::Ac3 => "AC3",
            OutputAudioCodec::Eac3 => "E-AC3",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioAction {
    Copy,
    Transcode {
        codec: OutputAudioCodec,
        bitrate_k: u32,
        channels: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioPurpose {
    Original,
    /// Original that had to be converted.
    Converted,
    /// Additional AAC stereo compatibility track.
    CompatibilityStereo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioOutput {
    pub source_type_index: u32,
    pub action: AudioAction,
    pub purpose: AudioPurpose,
    pub language: Option<String>,
    pub title: Option<String>,
    pub default: bool,
    pub forced: bool,
    /// Estimated bitrate (bps) of the output track; `None` if unknown.
    pub estimated_bitrate: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SidecarFormat {
    /// PGS → `.sup`
    Sup,
    /// VobSub / DVB → Matroska subtitle file `.mks`
    Mks,
    /// Text → `.srt`
    Srt,
}

impl SidecarFormat {
    pub fn extension(self) -> &'static str {
        match self {
            SidecarFormat::Sup => "sup",
            SidecarFormat::Mks => "mks",
            SidecarFormat::Srt => "srt",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubtitleAction {
    /// Text subtitle converted to `mov_text` inside the MP4.
    MovText,
    /// Written next to the result. `suffix` like `ger.forced`.
    Sidecar {
        format: SidecarFormat,
        suffix: String,
    },
    /// Not kept – always accompanied by a warning.
    Skip,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubtitleOutput {
    pub source_type_index: u32,
    pub codec: SubtitleCodec,
    pub action: SubtitleAction,
    pub language: Option<String>,
    pub title: Option<String>,
    pub default: bool,
    pub forced: bool,
}

/// Issues shown before encoding. Stored structured, translated on display.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PlanWarning {
    AudioConverted {
        track: u32,
        from: String,
        to: String,
    },
    AtmosLost {
        track: u32,
    },
    DtsXLost {
        track: u32,
    },
    ChannelsReduced {
        track: u32,
        from: u32,
        to: u32,
    },
    LosslessToLossy {
        track: u32,
        from: String,
    },
    LimitedClientSupport {
        track: u32,
        codec: String,
    },
    AssStylingLost {
        track: u32,
    },
    ForcedFlagViaSidecar {
        track: u32,
    },
    /// Found while exporting: the source track has no subtitle events at all,
    /// so there is nothing to export (common in samples or empty forced tracks).
    SubtitleStreamEmpty {
        track: u32,
    },
    BitmapSubtitleSidecar {
        track: u32,
        codec: String,
    },
    BitmapSubtitleSkipped {
        track: u32,
        codec: String,
    },
    UnsupportedSubtitleSkipped {
        track: u32,
        codec: String,
    },
    HdrToneMapped {
        format: String,
    },
    DolbyVisionMetadataDropped,
    Hdr10PlusMetadataDropped,
    HdrPreserveNotSupported,
    /// HDR is kept only when the video stream is copied.
    HdrPreserveNeedsCopy,
    BlackBarsCropped {
        from: Dimensions,
        to: Dimensions,
    },
    CoverArtDropped,
    DataStreamsDropped {
        count: usize,
    },
    UnknownDuration,
    MultipleVideoStreams {
        count: usize,
    },
}

impl PlanWarning {
    pub fn describe(&self) -> String {
        match self {
            PlanWarning::AudioConverted { track, from, to } => trf!(
                "Audio track {}: {} is converted to {} (cannot be kept unchanged in MP4)",
                track,
                from,
                to
            ),
            PlanWarning::AtmosLost { track } => trf!(
                "Audio track {}: Dolby Atmos object information is lost in the conversion",
                track
            ),
            PlanWarning::DtsXLost { track } => trf!(
                "Audio track {}: DTS:X object information is lost in the conversion",
                track
            ),
            PlanWarning::ChannelsReduced { track, from, to } => trf!(
                "Audio track {}: channels are downmixed from {} to {}",
                track,
                from,
                to
            ),
            PlanWarning::LosslessToLossy { track, from } => {
                trf!("Audio track {}: lossless {} is encoded lossy", track, from)
            }
            PlanWarning::LimitedClientSupport { track, codec } => trf!(
                "Audio track {}: {} is kept but not every client can direct-play it",
                track,
                codec
            ),
            PlanWarning::AssStylingLost { track } => {
                trf!("Subtitle {}: ASS/SSA styling is lost in mov_text", track)
            }
            PlanWarning::ForcedFlagViaSidecar { track } => trf!(
                "Subtitle {}: MP4 cannot store the forced flag – track title marked 'Forced' and exported as .forced.srt sidecar",
                track
            ),
            PlanWarning::SubtitleStreamEmpty { track } => trf!(
                "Subtitle {}: the source track contains no subtitle events – no sidecar file created",
                track
            ),
            PlanWarning::BitmapSubtitleSidecar { track, codec } => trf!(
                "Subtitle {}: {} (bitmap) is not MP4 compatible and is exported as sidecar file",
                track,
                codec
            ),
            PlanWarning::BitmapSubtitleSkipped { track, codec } => trf!(
                "Subtitle {}: {} (bitmap) is NOT kept (OCR planned for a later version)",
                track,
                codec
            ),
            PlanWarning::UnsupportedSubtitleSkipped { track, codec } => trf!(
                "Subtitle {}: format {} is unsupported and NOT kept",
                track,
                codec
            ),
            PlanWarning::HdrToneMapped { format } => {
                trf!("{} is tone mapped to SDR (BT.709)", format)
            }
            PlanWarning::DolbyVisionMetadataDropped => {
                tr!("Dolby Vision metadata is dropped; the base layer is converted").into()
            }
            PlanWarning::Hdr10PlusMetadataDropped => {
                tr!("HDR10+ dynamic metadata is dropped by the SDR conversion").into()
            }
            PlanWarning::HdrPreserveNotSupported => {
                tr!("Preserving HDR requires HEVC – converting to SDR").into()
            }
            PlanWarning::HdrPreserveNeedsCopy => tr!("HDR can only be kept when the video stream is copied unchanged – re-encoding converts to SDR").into(),
            PlanWarning::BlackBarsCropped { from, to } => {
                trf!("Black bars removed: {} → {}", from, to)
            }
            PlanWarning::CoverArtDropped => tr!("Embedded cover art is not kept").into(),
            PlanWarning::DataStreamsDropped { count } => trf!(
                "{} data/attachment stream(s) (e.g. fonts) are not kept",
                count
            ),
            PlanWarning::UnknownDuration => {
                tr!("Duration unknown – progress and validation are limited").into()
            }
            PlanWarning::MultipleVideoStreams { count } => {
                trf!("{} video streams present – only the first is kept", count)
            }
        }
    }
}

/// Reasons why a file cannot be processed safely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanBlocker {
    NoVideo,
    TonemapUnavailable,
    DolbyVisionProfile5,
    EncoderUnavailable(String),
}

impl PlanBlocker {
    pub fn describe(&self) -> String {
        match self {
            PlanBlocker::NoVideo => tr!("No video stream found").into(),
            PlanBlocker::TonemapUnavailable => tr!("HDR source but ffmpeg lacks zscale/tonemap – HDR metadata would be removed without converting the image")
            .into(),
            PlanBlocker::DolbyVisionProfile5 => tr!("Dolby Vision profile 5 has no HDR10/SDR base layer and cannot be converted correctly")
            .into(),
            PlanBlocker::EncoderUnavailable(e) => {
                trf!("Encoder {} is not available", e)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EncodePlan {
    pub profile_id: String,
    pub video: Option<VideoPlan>,
    pub audio: Vec<AudioOutput>,
    pub subtitles: Vec<SubtitleOutput>,
    pub warnings: Vec<PlanWarning>,
    pub blockers: Vec<PlanBlocker>,
    pub duration_s: Option<f64>,
    /// Estimated output size in bytes (estimate, not a guarantee).
    pub estimated_size: Option<u64>,
}

impl EncodePlan {
    pub fn is_blocked(&self) -> bool {
        !self.blockers.is_empty()
    }

    pub fn mov_text_count(&self) -> usize {
        self.subtitles
            .iter()
            .filter(|s| s.action == SubtitleAction::MovText)
            .count()
    }

    pub fn sidecars(&self) -> impl Iterator<Item = &SubtitleOutput> {
        self.subtitles
            .iter()
            .filter(|s| matches!(s.action, SubtitleAction::Sidecar { .. }))
    }

    pub fn video_is_copied(&self) -> bool {
        matches!(
            self.video.as_ref().map(|v| &v.action),
            Some(VideoAction::Copy)
        )
    }
}

/// Bits per pixel per frame – a codec-independent density measure.
pub fn bits_per_pixel(bitrate: u64, dims: Dimensions, fps: f64) -> Option<f64> {
    let denom = dims.pixels() as f64 * fps;
    (denom > 0.0).then(|| bitrate as f64 / denom)
}

/// Heuristic H.264 bitrate model for a CRF encode. Used for **estimates
/// only** – actual sizes depend heavily on content (grain, motion).
///
/// Base: ~0.075 bpp at 1080p / CRF 20, lower resolutions need slightly more
/// bits per pixel, +6 CRF ≈ half the bitrate, HEVC ≈ 35 % smaller.
pub fn estimate_video_bitrate(dims: Dimensions, fps: f64, crf: u8, codec: TargetVideoCodec) -> u64 {
    let pixels = dims.pixels().max(1) as f64;
    let res_factor = (1920.0 * 1080.0 / pixels).powf(0.3);
    let crf_factor = 2f64.powf((20.0 - f64::from(crf)) / 6.0);
    let codec_factor = match codec {
        TargetVideoCodec::H264 => 1.0,
        TargetVideoCodec::Hevc => 0.65,
    };
    (0.075 * res_factor * crf_factor * codec_factor * pixels * fps.max(1.0)) as u64
}

fn copyable_in_mp4(codec: &AudioCodec, strategy: AudioStrategy) -> bool {
    match strategy {
        AudioStrategy::Compatibility => {
            matches!(
                codec,
                AudioCodec::Aac | AudioCodec::Ac3 | AudioCodec::Eac3 | AudioCodec::Mp3
            )
        }
        AudioStrategy::PreserveOriginal => matches!(
            codec,
            AudioCodec::Aac
                | AudioCodec::Ac3
                | AudioCodec::Eac3
                | AudioCodec::Mp3
                | AudioCodec::Alac
                | AudioCodec::Dts
                | AudioCodec::DtsHdMa
                | AudioCodec::DtsHdHra
        ),
    }
}

fn typical_bitrate(a: &AudioStream) -> u64 {
    a.bit_rate.unwrap_or_else(|| {
        let ch = u64::from(a.channels_or_default());
        match a.codec {
            AudioCodec::TrueHd | AudioCodec::DtsHdMa => 3_500_000,
            AudioCodec::DtsHdHra | AudioCodec::Dts => 1_509_000,
            AudioCodec::Pcm | AudioCodec::Flac | AudioCodec::Alac => 600_000 * ch,
            AudioCodec::Ac3 | AudioCodec::Eac3 if ch > 2 => 640_000,
            _ => 96_000 * ch,
        }
    })
}

fn sidecar_suffix(s: &SubtitleStream, used: &mut Vec<String>) -> String {
    let mut base = s.language.clone().unwrap_or_else(|| "und".into());
    if s.forced {
        base.push_str(".forced");
    }
    if s.hearing_impaired {
        base.push_str(".sdh");
    }
    let mut suffix = base.clone();
    let mut n = 2;
    while used.contains(&suffix) {
        suffix = format!("{base}.{n}");
        n += 1;
    }
    used.push(suffix.clone());
    suffix
}

/// Build the encode plan for `media` with `profile`.
pub fn build_plan(
    media: &MediaFile,
    profile: &EncodingProfile,
    caps: &PlanCapabilities,
) -> EncodePlan {
    let mut warnings = Vec::new();
    let mut blockers = Vec::new();

    let real_videos: Vec<&VideoStream> = media.video.iter().filter(|v| !v.attached_pic).collect();
    if real_videos.len() > 1 {
        warnings.push(PlanWarning::MultipleVideoStreams {
            count: real_videos.len(),
        });
    }
    if media.video.iter().any(|v| v.attached_pic) {
        warnings.push(PlanWarning::CoverArtDropped);
    }
    if !media.other_streams.is_empty() {
        warnings.push(PlanWarning::DataStreamsDropped {
            count: media.other_streams.len(),
        });
    }
    if media.duration_s().is_none() {
        warnings.push(PlanWarning::UnknownDuration);
    }

    let video = match media.primary_video() {
        Some(v) => Some(plan_video(
            media,
            v,
            profile,
            caps,
            &mut warnings,
            &mut blockers,
        )),
        None => {
            blockers.push(PlanBlocker::NoVideo);
            None
        }
    };

    let audio = plan_audio(&media.audio, profile, &mut warnings);
    let subtitles = plan_subtitles(&media.subtitles, profile, &mut warnings);

    let estimated_size = estimate_size(media, video.as_ref(), &audio);

    EncodePlan {
        profile_id: profile.id.clone(),
        video,
        audio,
        subtitles,
        warnings,
        blockers,
        duration_s: media.duration_s(),
        estimated_size,
    }
}

fn plan_video(
    media: &MediaFile,
    v: &VideoStream,
    profile: &EncodingProfile,
    caps: &PlanCapabilities,
    warnings: &mut Vec<PlanWarning>,
    blockers: &mut Vec<PlanBlocker>,
) -> VideoPlan {
    let settings = &profile.video;
    let source = resolution::oriented(v.width, v.height, v.rotation);
    let resolution = resolution::compute_target_cropped(
        source,
        v.crop.filter(|_| settings.auto_crop),
        Dimensions::new(settings.max_width, settings.max_height),
    );
    let mut reasons = Vec::new();

    let codec_ok = match settings.codec {
        TargetVideoCodec::H264 => v.codec == VideoCodec::H264,
        TargetVideoCodec::Hevc => v.codec == VideoCodec::Hevc || v.codec == VideoCodec::H264,
    };
    if !codec_ok {
        reasons.push(VideoReason::Codec(v.codec.label().to_string()));
    }
    if settings.codec == TargetVideoCodec::H264
        && let Some(depth) = v.bit_depth.filter(|d| *d > 8)
    {
        reasons.push(VideoReason::BitDepth(depth));
    }
    if v.is_yuv420() == Some(false) {
        reasons.push(VideoReason::PixelFormat(
            v.pix_fmt.clone().unwrap_or_default(),
        ));
    }

    match resolution.op {
        ResizeOp::Downscale => reasons.push(VideoReason::Resolution(source)),
        ResizeOp::CropToEven => reasons.push(VideoReason::OddDimensions),
        ResizeOp::Keep => {}
    }
    let deinterlace = settings.deinterlace && v.interlaced == Some(true);
    if deinterlace {
        reasons.push(VideoReason::Interlaced);
    }

    // HDR is only kept by copying the stream: re-encoding writes 8-bit
    // BT.709 and would silently drop it, so every encode tone maps.
    let hdr_source = v.hdr.is_hdr();
    let preserve_hdr = hdr_source
        && profile.hdr.strategy == HdrStrategy::Preserve
        && settings.codec == TargetVideoCodec::Hevc;
    if hdr_source
        && profile.hdr.strategy == HdrStrategy::Preserve
        && settings.codec != TargetVideoCodec::Hevc
    {
        warnings.push(PlanWarning::HdrPreserveNotSupported);
    }
    if hdr_source && !preserve_hdr {
        reasons.push(VideoReason::Hdr(v.hdr.label()));
    }

    let bpp = media
        .video_bitrate()
        .zip(v.fps())
        .and_then(|(br, fps)| bits_per_pixel(br, source, fps));
    if reasons.is_empty()
        && let Some(bpp) = bpp.filter(|b| *b > settings.copy_max_bpp)
    {
        reasons.push(VideoReason::HighBitrate { bpp });
    }
    if reasons.is_empty() && !settings.allow_copy {
        reasons.push(VideoReason::ProfileForcesEncode);
    }
    if preserve_hdr && !reasons.is_empty() {
        warnings.push(PlanWarning::HdrPreserveNeedsCopy);
        reasons.push(VideoReason::Hdr(v.hdr.label()));
    }

    let mut hdr = HdrConversion::None;
    if hdr_source && !reasons.is_empty() {
        if !(caps.zscale && caps.tonemap) {
            blockers.push(PlanBlocker::TonemapUnavailable);
        }
        match &v.hdr {
            HdrFormat::DolbyVision(dv) => {
                if dv.compatibility == DvCompatibility::None || dv.profile == Some(5) {
                    blockers.push(PlanBlocker::DolbyVisionProfile5);
                }
                warnings.push(PlanWarning::DolbyVisionMetadataDropped);
            }
            HdrFormat::Hdr10Plus => warnings.push(PlanWarning::Hdr10PlusMetadataDropped),
            _ => {}
        }
        warnings.push(PlanWarning::HdrToneMapped {
            format: v.hdr.label(),
        });
        hdr = HdrConversion::TonemapToSdr {
            source: v.hdr.clone(),
        };
    }

    if !caps.libx264 && settings.codec == TargetVideoCodec::H264 && !reasons.is_empty() {
        blockers.push(PlanBlocker::EncoderUnavailable("libx264".into()));
    }
    if !caps.libx265 && settings.codec == TargetVideoCodec::Hevc && !reasons.is_empty() {
        blockers.push(PlanBlocker::EncoderUnavailable("libx265".into()));
    }
    if let Some(crop) = resolution.crop.filter(|_| !reasons.is_empty()) {
        warnings.push(PlanWarning::BlackBarsCropped {
            from: source,
            to: crop.dimensions(),
        });
    }

    let action = if reasons.is_empty() {
        VideoAction::Copy
    } else {
        VideoAction::Encode {
            codec: settings.codec,
            crf: settings.crf,
            resolution,
            hdr,
            deinterlace,
        }
    };
    VideoPlan {
        source_type_index: v.type_index,
        source,
        storage: Dimensions::new(v.width, v.height),
        action,
        reasons,
    }
}

fn plan_audio(
    tracks: &[AudioStream],
    profile: &EncodingProfile,
    warnings: &mut Vec<PlanWarning>,
) -> Vec<AudioOutput> {
    let settings = &profile.audio;
    let mut outputs: Vec<AudioOutput> = Vec::new();

    for a in tracks {
        let track = a.type_index + 1;
        let channels = a.channels_or_default();
        if copyable_in_mp4(&a.codec, settings.strategy) {
            if a.codec.is_dts_family() {
                warnings.push(PlanWarning::LimitedClientSupport {
                    track,
                    codec: a.codec.label().into(),
                });
            }
            outputs.push(AudioOutput {
                source_type_index: a.type_index,
                action: AudioAction::Copy,
                purpose: AudioPurpose::Original,
                language: a.language.clone(),
                title: a.title.clone(),
                default: a.default,
                forced: a.forced,
                estimated_bitrate: Some(typical_bitrate(a)),
            });
            continue;
        }

        // Needs conversion.
        let (codec, bitrate_k, out_channels) = if channels > 2 {
            let mc = settings.multichannel_codec;
            let codec = match mc {
                MultichannelCodec::Ac3 => OutputAudioCodec::Ac3,
                MultichannelCodec::Eac3 => OutputAudioCodec::Eac3,
                MultichannelCodec::Aac => OutputAudioCodec::Aac,
            };
            (
                codec,
                settings.multichannel_bitrate_k,
                channels.min(mc.max_channels()),
            )
        } else {
            (
                OutputAudioCodec::Aac,
                settings.stereo_bitrate_k,
                channels.max(1),
            )
        };
        warnings.push(PlanWarning::AudioConverted {
            track,
            from: a.codec.label().into(),
            to: format!("{} {}ch", codec.label(), out_channels),
        });
        if a.atmos.is_present() {
            warnings.push(PlanWarning::AtmosLost { track });
        }
        if a.dts_x.is_present() {
            warnings.push(PlanWarning::DtsXLost { track });
        }
        if out_channels < channels {
            warnings.push(PlanWarning::ChannelsReduced {
                track,
                from: channels,
                to: out_channels,
            });
        }
        if a.codec.is_lossless() {
            warnings.push(PlanWarning::LosslessToLossy {
                track,
                from: a.codec.label().into(),
            });
        }
        outputs.push(AudioOutput {
            source_type_index: a.type_index,
            action: AudioAction::Transcode {
                codec,
                bitrate_k,
                channels: out_channels,
            },
            purpose: AudioPurpose::Converted,
            language: a.language.clone(),
            title: a.title.clone(),
            default: a.default,
            forced: a.forced,
            estimated_bitrate: Some(u64::from(bitrate_k) * 1000),
        });
    }

    add_compat_stereo(tracks, settings, &mut outputs);
    normalize_audio_defaults(&mut outputs, settings.compat_stereo_default);
    outputs
}

/// Add AAC-LC stereo compatibility tracks where none exists yet.
fn add_compat_stereo(
    tracks: &[AudioStream],
    settings: &super::profile::AudioSettings,
    outputs: &mut Vec<AudioOutput>,
) {
    if settings.compat_stereo == CompatStereoMode::Never {
        return;
    }
    let lang_key = |l: &Option<String>| l.clone().unwrap_or_else(|| "und".into());
    let primary_lang = tracks
        .iter()
        .find(|a| a.default)
        .or_else(|| tracks.first())
        .map(|a| lang_key(&a.language));

    let mut languages: Vec<String> = Vec::new();
    for a in tracks.iter().filter(|a| !a.commentary) {
        let l = lang_key(&a.language);
        if !languages.contains(&l) {
            languages.push(l);
        }
    }
    if settings.compat_stereo == CompatStereoMode::PrimaryLanguage {
        languages.retain(|l| Some(l) == primary_lang.as_ref());
    }

    for lang in languages {
        let has_stereo_aac = outputs.iter().any(|o| {
            lang_key(&o.language) == lang
                && match &o.action {
                    AudioAction::Transcode {
                        codec, channels, ..
                    } => *codec == OutputAudioCodec::Aac && *channels <= 2,
                    AudioAction::Copy => tracks
                        .iter()
                        .find(|t| t.type_index == o.source_type_index)
                        .is_some_and(|t| t.is_aac_lc() && t.channels_or_default() <= 2),
                }
        });
        if has_stereo_aac {
            continue;
        }
        // Source: default track of that language, otherwise the one with most channels.
        let Some(src) = tracks
            .iter()
            .filter(|a| !a.commentary && lang_key(&a.language) == lang)
            .max_by_key(|a| (a.default, a.channels_or_default()))
        else {
            continue;
        };
        // Insert directly after the last output derived from this language.
        let pos = outputs
            .iter()
            .rposition(|o| lang_key(&o.language) == lang)
            .map_or(outputs.len(), |p| p + 1);
        outputs.insert(
            pos,
            AudioOutput {
                source_type_index: src.type_index,
                action: AudioAction::Transcode {
                    codec: OutputAudioCodec::Aac,
                    bitrate_k: settings.stereo_bitrate_k,
                    channels: 2,
                },
                purpose: AudioPurpose::CompatibilityStereo,
                language: src.language.clone(),
                title: Some("AAC 2.0".into()),
                default: false,
                forced: false,
                estimated_bitrate: Some(u64::from(settings.stereo_bitrate_k) * 1000),
            },
        );
    }
}

/// Exactly one default audio track.
fn normalize_audio_defaults(outputs: &mut [AudioOutput], prefer_compat: bool) {
    if outputs.is_empty() {
        return;
    }
    let original_default = outputs
        .iter()
        .position(|o| o.default && o.purpose != AudioPurpose::CompatibilityStereo)
        .unwrap_or(0);
    let chosen = if prefer_compat {
        let lang = outputs[original_default].language.clone();
        outputs
            .iter()
            .position(|o| o.purpose == AudioPurpose::CompatibilityStereo && o.language == lang)
            .unwrap_or(original_default)
    } else {
        original_default
    };
    for (i, o) in outputs.iter_mut().enumerate() {
        o.default = i == chosen;
    }
}

fn plan_subtitles(
    tracks: &[SubtitleStream],
    profile: &EncodingProfile,
    warnings: &mut Vec<PlanWarning>,
) -> Vec<SubtitleOutput> {
    let mut outputs = Vec::new();
    let mut used_suffixes = Vec::new();
    for s in tracks {
        let track = s.type_index + 1;
        let base = |action| SubtitleOutput {
            source_type_index: s.type_index,
            codec: s.codec.clone(),
            action,
            language: s.language.clone(),
            title: s.title.clone(),
            default: s.default,
            forced: s.forced,
        };
        match s.kind() {
            SubtitleKind::Text => {
                if matches!(s.codec, SubtitleCodec::Ass | SubtitleCodec::Ssa) {
                    warnings.push(PlanWarning::AssStylingLost { track });
                }
                let mut embedded = base(SubtitleAction::MovText);
                if s.forced {
                    // MP4/mov_text has no reliable forced flag: mark the title
                    // (parsed by Plex/Jellyfin) and export a `.forced.srt`.
                    warnings.push(PlanWarning::ForcedFlagViaSidecar { track });
                    embedded.title = Some(match &s.title {
                        Some(t) if t.to_lowercase().contains("forced") => t.clone(),
                        Some(t) => format!("{t} (Forced)"),
                        None => "Forced".into(),
                    });
                }
                outputs.push(embedded);
                if profile.subtitles.text_sidecar || s.forced {
                    outputs.push(base(SubtitleAction::Sidecar {
                        format: SidecarFormat::Srt,
                        suffix: sidecar_suffix(s, &mut used_suffixes),
                    }));
                }
            }
            SubtitleKind::Bitmap => match profile.subtitles.bitmap {
                BitmapSubtitleAction::Sidecar => {
                    warnings.push(PlanWarning::BitmapSubtitleSidecar {
                        track,
                        codec: s.codec.label().into(),
                    });
                    let format = if s.codec == SubtitleCodec::Pgs {
                        SidecarFormat::Sup
                    } else {
                        SidecarFormat::Mks
                    };
                    outputs.push(base(SubtitleAction::Sidecar {
                        format,
                        suffix: sidecar_suffix(s, &mut used_suffixes),
                    }));
                }
                BitmapSubtitleAction::Skip => {
                    warnings.push(PlanWarning::BitmapSubtitleSkipped {
                        track,
                        codec: s.codec.label().into(),
                    });
                    outputs.push(base(SubtitleAction::Skip));
                }
            },
            SubtitleKind::Unknown => {
                warnings.push(PlanWarning::UnsupportedSubtitleSkipped {
                    track,
                    codec: s.codec_name.clone(),
                });
                outputs.push(base(SubtitleAction::Skip));
            }
        }
    }
    // Only one default subtitle; forced flags are kept as-is.
    let mut seen_default = false;
    for o in outputs
        .iter_mut()
        .filter(|o| o.action == SubtitleAction::MovText)
    {
        if o.default {
            o.default = !seen_default;
            seen_default = true;
        }
    }
    outputs
}

fn estimate_size(
    media: &MediaFile,
    video: Option<&VideoPlan>,
    audio: &[AudioOutput],
) -> Option<u64> {
    estimate_parts(media, video, audio).map(SizeParts::total)
}

/// Size estimate split into the re-encoded video part (which a calibration
/// from real results may correct) and everything else (audio, overhead).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SizeParts {
    pub video: u64,
    pub other: u64,
}

impl SizeParts {
    pub fn total(self) -> u64 {
        self.video + self.other
    }
}

/// Estimate of the output size, split into video and the rest.
pub fn estimate_parts(
    media: &MediaFile,
    video: Option<&VideoPlan>,
    audio: &[AudioOutput],
) -> Option<SizeParts> {
    let duration = media.duration_s()?;
    let v = video?;
    let fps = media
        .primary_video()
        .and_then(VideoStream::fps)
        .unwrap_or(25.0);
    let video_bitrate = match &v.action {
        VideoAction::Copy => media.video_bitrate()?,
        VideoAction::Encode {
            codec,
            crf,
            resolution,
            ..
        } => {
            let est = estimate_video_bitrate(resolution.output, fps, *crf, *codec);
            // A CRF encode will rarely exceed an efficient source's bitrate;
            // clamp against the source adjusted for codec efficiency.
            match media.video_bitrate() {
                Some(src) => {
                    let eff = media
                        .primary_video()
                        .map_or(1.0, |pv| pv.codec.efficiency_vs_h264());
                    let scale = resolution.output.pixels() as f64 / v.source.pixels().max(1) as f64;
                    let ceiling = (src as f64 * eff * scale.min(1.0) * 1.1) as u64;
                    est.min(ceiling.max(1))
                }
                None => est,
            }
        }
    };
    let audio_bitrate: u64 = audio.iter().filter_map(|a| a.estimated_bitrate).sum();
    // ~1 % container overhead.
    let bytes = |bitrate: u64| (bitrate as f64 / 8.0 * duration * 1.01) as u64;
    Some(SizeParts {
        video: bytes(video_bitrate),
        other: bytes(audio_bitrate),
    })
}
