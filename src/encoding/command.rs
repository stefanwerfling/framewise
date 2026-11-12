//! ffmpeg argument construction.
//!
//! Arguments are built as a list (`Vec<OsString>`) and passed directly to the
//! process – never through a shell – so file names can contain any character.
//!
//! Encoding decisions documented here:
//! * Streams are mapped explicitly (`-map 0:v:N` …); nothing is mapped implicitly.
//! * Dispositions and language/title tags are always written explicitly, so the
//!   output reflects the plan and not ffmpeg's defaults.
//! * HDR → SDR: downscale first (cheap at 1080p), then linearise with zscale,
//!   tone map in float RGB, convert to BT.709 and tag the output accordingly.
//!   Input transfer/primaries are set explicitly because many files carry
//!   incomplete colour tags.
//! * H.264 level is raised to 4.2 for >30 fps at 1080p (level 4.1 would be
//!   signalled incorrectly).

use std::ffi::OsString;
use std::path::Path;

use super::plan::{
    AudioAction, EncodePlan, HdrConversion, SidecarFormat, SubtitleAction, SubtitleOutput,
    VideoAction,
};
use super::profile::{EncodingProfile, TargetVideoCodec};
use crate::analyzer::resolution::{Dimensions, ResizeOp, TargetResolution};
use crate::core::config::HwBackend;
use crate::encoding::hardware::{encoder_name, hwaccel_name};
use crate::media::model::{HdrFormat, MediaFile};

/// Concrete encoder/decoder choice for one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderSetup {
    /// `None` = CPU (libx264/libx265).
    pub hw_encode: Option<HwBackend>,
    /// `None` = software decoding.
    pub hw_decode: Option<HwBackend>,
    pub threads: usize,
    pub vaapi_device: String,
    pub gpu_index: u32,
}

impl EncoderSetup {
    pub fn cpu() -> Self {
        Self {
            hw_encode: None,
            hw_decode: None,
            threads: 0,
            vaapi_device: "/dev/dri/renderD128".into(),
            gpu_index: 0,
        }
    }

    pub fn encoder_label(&self, codec: TargetVideoCodec) -> String {
        let backend = self.hw_encode.unwrap_or(HwBackend::Cpu);
        let name = encoder_name(backend, codec).unwrap_or("?");
        match self.hw_decode.and_then(hwaccel_name) {
            Some(dec) => format!("{name} + {dec}"),
            None => name.to_string(),
        }
    }
}

/// Optional time window (for quality samples).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Window {
    pub start_s: f64,
    pub duration_s: f64,
}

/// Builder for argument lists.
#[derive(Debug, Default)]
pub struct Args(Vec<OsString>);

impl Args {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn arg(&mut self, a: impl Into<OsString>) -> &mut Self {
        self.0.push(a.into());
        self
    }

    pub fn pair(&mut self, a: impl Into<OsString>, b: impl Into<OsString>) -> &mut Self {
        self.arg(a).arg(b)
    }

    pub fn into_vec(self) -> Vec<OsString> {
        self.0
    }
}

fn base_args(args: &mut Args) {
    args.arg("-hide_banner")
        .arg("-nostdin")
        // never overwrite – outputs always go to fresh temp paths
        .arg("-n")
        .pair("-loglevel", "warning")
        .pair("-progress", "pipe:1")
        .arg("-nostats");
}

fn h264_level(dims: Dimensions, fps: f64, configured: &str) -> String {
    let needs_42 = dims.pixels() > 1280 * 720 && fps > 30.5;
    if needs_42 && configured.parse::<f64>().is_ok_and(|l| l < 4.2) {
        "4.2".into()
    } else {
        configured.to_string()
    }
}

/// Build the `-vf` chain (CPU filters; hardware upload appended if needed).
pub fn video_filters(
    resolution: &TargetResolution,
    hdr: &HdrConversion,
    deinterlace: bool,
    profile: &EncodingProfile,
    hw_encode: Option<HwBackend>,
) -> Option<String> {
    let mut chain: Vec<String> = Vec::new();
    if deinterlace {
        chain.push("bwdif=mode=send_frame:parity=auto:deint=interlaced".into());
    }
    if let Some(c) = resolution.crop {
        chain.push(format!("crop={}:{}:{}:{}", c.width, c.height, c.x, c.y));
    }
    let out = resolution.output;
    match resolution.op {
        ResizeOp::Keep => {}
        ResizeOp::CropToEven => chain.push(format!("crop={}:{}:0:0", out.width, out.height)),
        ResizeOp::Downscale => {
            if matches!(hdr, HdrConversion::TonemapToSdr { .. }) {
                chain.push(format!(
                    "zscale=w={}:h={}:f=spline36",
                    out.width, out.height
                ));
            } else {
                chain.push(format!("scale={}:{}:flags=lanczos", out.width, out.height));
            }
        }
    }
    if let HdrConversion::TonemapToSdr { source } = hdr {
        let tin = match source {
            HdrFormat::Hlg => "arib-std-b67",
            _ => "smpte2084",
        };
        chain.push(format!(
            "zscale=tin={tin}:min=bt2020nc:pin=bt2020:rin=tv:t=linear:npl={}",
            profile.hdr.npl
        ));
        chain.push("format=gbrpf32le".into());
        chain.push("zscale=p=bt709".into());
        chain.push(format!(
            "tonemap=tonemap={}:desat={}",
            profile.hdr.algorithm.ffmpeg_name(),
            profile.hdr.desat
        ));
        chain.push("zscale=t=bt709:m=bt709:r=tv".into());
    }
    match hw_encode {
        Some(HwBackend::Vaapi) => {
            chain.push("format=nv12".into());
            chain.push("hwupload".into());
        }
        Some(HwBackend::IntelQsv) => chain.push("format=nv12".into()),
        _ => chain.push("format=yuv420p".into()),
    }
    Some(chain.join(","))
}

/// Per-run video encoder parameters.
struct VideoEncode<'a> {
    codec: TargetVideoCodec,
    crf: u8,
    out: Dimensions,
    fps: f64,
    profile: &'a EncodingProfile,
    setup: &'a EncoderSetup,
    /// Used for encoders without a constant-quality mode.
    estimated_bitrate: u64,
}

fn video_encoder_args(args: &mut Args, p: &VideoEncode<'_>) {
    let VideoEncode {
        codec,
        crf,
        out,
        fps,
        profile,
        setup,
        estimated_bitrate,
    } = *p;
    let v = &profile.video;
    let backend = setup.hw_encode.unwrap_or(HwBackend::Cpu);
    let encoder = encoder_name(backend, codec).unwrap_or("libx264");
    args.pair("-c:v", encoder);
    let q = crf.to_string();
    match backend {
        HwBackend::Cpu | HwBackend::Auto => {
            args.pair("-preset", v.preset.as_str())
                .pair("-crf", q.as_str());
            if let Some(tune) = v.tune.as_deref().filter(|t| !t.is_empty()) {
                args.pair("-tune", tune);
            }
            if setup.threads > 0 {
                args.pair("-threads", setup.threads.to_string());
            }
        }
        HwBackend::Nvidia => {
            args.pair("-preset", "p5")
                .pair("-tune", "hq")
                .pair("-rc", "vbr")
                .pair("-cq", (crf + 1).to_string())
                .pair("-b:v", "0")
                .pair("-gpu", setup.gpu_index.to_string());
        }
        HwBackend::IntelQsv => {
            args.pair("-preset", "slower")
                .pair("-global_quality", q.as_str());
        }
        HwBackend::Vaapi => {
            args.pair("-qp", q.as_str());
        }
        HwBackend::AmdAmf => {
            args.pair("-quality", "quality")
                .pair("-rc", "cqp")
                .pair("-qp_i", q.as_str())
                .pair("-qp_p", q.as_str())
                .pair("-qp_b", q.as_str());
        }
        HwBackend::VideoToolbox => {
            // VideoToolbox has no portable constant-quality mode: use the
            // estimated bitrate of the CRF model.
            args.pair("-b:v", format!("{}k", (estimated_bitrate / 1000).max(500)));
        }
    }
    match codec {
        TargetVideoCodec::H264 => {
            args.pair("-profile:v", v.h264_profile.as_str());
            if matches!(backend, HwBackend::Cpu | HwBackend::Auto) {
                args.pair("-level:v", h264_level(out, fps, &v.h264_level));
            }
        }
        TargetVideoCodec::Hevc => {
            args.pair("-tag:v", "hvc1");
            // x265 ignores -threads and sizes its own pool to all cores.
            if matches!(backend, HwBackend::Cpu | HwBackend::Auto) {
                let mut params = String::from("log-level=error");
                if setup.threads > 0 {
                    params.push_str(&format!(":pools={}", setup.threads));
                }
                args.pair("-x265-params", params);
            }
        }
    }
    if !matches!(backend, HwBackend::Vaapi | HwBackend::IntelQsv) {
        args.pair("-pix_fmt", "yuv420p");
    }
}

fn disposition(default: bool, forced: bool) -> &'static str {
    match (default, forced) {
        (true, true) => "default+forced",
        (true, false) => "default",
        (false, true) => "forced",
        (false, false) => "0",
    }
}

/// Inputs for [`build_encode_args`].
pub struct EncodeCommand<'a> {
    pub media: &'a MediaFile,
    pub plan: &'a EncodePlan,
    pub profile: &'a EncodingProfile,
    pub setup: &'a EncoderSetup,
    pub output: &'a Path,
    /// Override the CRF (smart encoding).
    pub crf: Option<u8>,
    /// Only encode this window, video only (quality samples).
    pub window: Option<Window>,
}

/// Build the complete ffmpeg argument list for the main encode.
pub fn build_encode_args(cmd: &EncodeCommand<'_>) -> Vec<OsString> {
    let mut args = Args::new();
    base_args(&mut args);
    let sample = cmd.window.is_some();

    if let Some(dec) = cmd.setup.hw_decode.and_then(hwaccel_name) {
        args.pair("-hwaccel", dec);
    }
    if cmd.setup.hw_encode == Some(HwBackend::Vaapi) {
        args.pair("-vaapi_device", cmd.setup.vaapi_device.as_str());
    }
    // Large probe window: PGS/late-starting streams in big remuxes.
    args.pair("-analyzeduration", "100M")
        .pair("-probesize", "100M");
    if let Some(w) = cmd.window {
        args.pair("-ss", format!("{:.3}", w.start_s))
            .pair("-t", format!("{:.3}", w.duration_s));
    }
    if cmd.setup.threads > 0 {
        // limit decoder and filter threads too, not only the encoder –
        // otherwise decoding still occupies every core
        args.pair("-threads", cmd.setup.threads.to_string())
            .pair("-filter_threads", cmd.setup.threads.to_string());
    }
    args.arg("-i").arg(cmd.media.absolute_path.as_os_str());

    // ---- video
    if let Some(v) = &cmd.plan.video {
        args.pair("-map", format!("0:v:{}", v.source_type_index));
        match &v.action {
            VideoAction::Copy => {
                args.pair("-c:v", "copy");
                if cmd
                    .media
                    .primary_video()
                    .is_some_and(|pv| pv.codec == crate::media::model::VideoCodec::Hevc)
                {
                    args.pair("-tag:v", "hvc1");
                }
            }
            VideoAction::Encode {
                codec,
                crf,
                resolution,
                hdr,
                deinterlace,
            } => {
                if let Some(filters) = video_filters(
                    resolution,
                    hdr,
                    *deinterlace,
                    cmd.profile,
                    cmd.setup.hw_encode,
                ) {
                    args.pair("-vf", filters);
                }
                let fps = cmd
                    .media
                    .primary_video()
                    .and_then(|pv| pv.fps())
                    .unwrap_or(25.0);
                let est = super::plan::estimate_video_bitrate(resolution.output, fps, *crf, *codec);
                video_encoder_args(
                    &mut args,
                    &VideoEncode {
                        codec: *codec,
                        crf: cmd.crf.unwrap_or(*crf),
                        out: resolution.output,
                        fps,
                        profile: cmd.profile,
                        setup: cmd.setup,
                        estimated_bitrate: est,
                    },
                );
                if matches!(hdr, HdrConversion::TonemapToSdr { .. }) {
                    args.pair("-color_primaries", "bt709")
                        .pair("-color_trc", "bt709")
                        .pair("-colorspace", "bt709")
                        .pair("-color_range", "tv");
                }
            }
        }
        args.pair("-disposition:v:0", "default");
    }

    if sample {
        args.arg("-an").arg("-sn").arg("-dn");
        args.pair("-f", "matroska").arg(cmd.output.as_os_str());
        return args.into_vec();
    }

    // ---- audio
    for (k, a) in cmd.plan.audio.iter().enumerate() {
        args.pair("-map", format!("0:a:{}", a.source_type_index));
        match &a.action {
            AudioAction::Copy => {
                args.pair(format!("-c:a:{k}"), "copy");
            }
            AudioAction::Transcode {
                codec,
                bitrate_k,
                channels,
            } => {
                args.pair(format!("-c:a:{k}"), codec.ffmpeg_encoder())
                    .pair(format!("-b:a:{k}"), format!("{bitrate_k}k"))
                    .pair(format!("-ac:a:{k}"), channels.to_string());
            }
        }
        args.pair(
            format!("-metadata:s:a:{k}"),
            format!("language={}", a.language.as_deref().unwrap_or("und")),
        );
        let title = a.title.as_deref().unwrap_or("");
        args.pair(format!("-metadata:s:a:{k}"), format!("title={title}"));
        // MP4 stores track names as handler name (read by Plex/Jellyfin).
        if !title.is_empty() {
            args.pair(
                format!("-metadata:s:a:{k}"),
                format!("handler_name={title}"),
            );
        }
        args.pair(
            format!("-disposition:a:{k}"),
            disposition(a.default, a.forced),
        );
    }

    // ---- subtitles (mov_text only; sidecars are separate runs)
    for (k, s) in cmd
        .plan
        .subtitles
        .iter()
        .filter(|s| s.action == SubtitleAction::MovText)
        .enumerate()
    {
        args.pair("-map", format!("0:s:{}", s.source_type_index));
        args.pair(format!("-c:s:{k}"), "mov_text");
        args.pair(
            format!("-metadata:s:s:{k}"),
            format!("language={}", s.language.as_deref().unwrap_or("und")),
        );
        let title = s.title.as_deref().unwrap_or("");
        args.pair(format!("-metadata:s:s:{k}"), format!("title={title}"));
        // MP4 stores track names as handler name (read by Plex/Jellyfin).
        if !title.is_empty() {
            args.pair(
                format!("-metadata:s:s:{k}"),
                format!("handler_name={title}"),
            );
        }
        args.pair(
            format!("-disposition:s:{k}"),
            disposition(s.default, s.forced),
        );
    }

    args.pair("-map_metadata", "0")
        .pair("-map_chapters", "0")
        .pair("-max_muxing_queue_size", "4096")
        .pair("-movflags", "+faststart")
        .pair("-f", "mp4")
        .arg(cmd.output.as_os_str());
    args.into_vec()
}

/// Extract one subtitle stream to a sidecar file (stream copy, or text → SRT).
/// Video encode parameters of a plan, if it re-encodes on the CPU – the only
/// case where a pre-filtered reference yields exactly the encoder input
/// (hardware paths add upload/format filters of their own).
fn cpu_video_encode<'a>(cmd: &EncodeCommand<'a>) -> Option<VideoEncode<'a>> {
    if !matches!(cmd.setup.hw_encode, None | Some(HwBackend::Cpu)) {
        return None;
    }
    let v = cmd.plan.video.as_ref()?;
    let VideoAction::Encode {
        codec,
        crf,
        resolution,
        ..
    } = &v.action
    else {
        return None;
    };
    let fps = cmd
        .media
        .primary_video()
        .and_then(|pv| pv.fps())
        .unwrap_or(25.0);
    Some(VideoEncode {
        codec: *codec,
        crf: cmd.crf.unwrap_or(*crf),
        out: resolution.output,
        fps,
        profile: cmd.profile,
        setup: cmd.setup,
        estimated_bitrate: super::plan::estimate_video_bitrate(
            resolution.output,
            fps,
            *crf,
            *codec,
        ),
    })
}

/// Lossless (FFV1) copy of one source window after the plan's video filters
/// (deinterlace/scale/tonemap/format): the exact frames the CPU encoder would
/// receive. Quality search encodes and measures against this file, so the
/// source is read and filtered once instead of twice per CRF step.
/// `None` when the plan does not re-encode on the CPU.
pub fn build_reference_args(
    cmd: &EncodeCommand<'_>,
    window: Window,
    output: &Path,
) -> Option<Vec<OsString>> {
    cpu_video_encode(cmd)?;
    let v = cmd.plan.video.as_ref()?;
    let VideoAction::Encode {
        resolution,
        hdr,
        deinterlace,
        ..
    } = &v.action
    else {
        return None;
    };
    let mut args = Args::new();
    base_args(&mut args);
    args.pair("-analyzeduration", "100M")
        .pair("-probesize", "100M")
        .pair("-ss", format!("{:.3}", window.start_s))
        .pair("-t", format!("{:.3}", window.duration_s));
    if cmd.setup.threads > 0 {
        args.pair("-threads", cmd.setup.threads.to_string())
            .pair("-filter_threads", cmd.setup.threads.to_string());
    }
    args.arg("-i").arg(cmd.media.absolute_path.as_os_str());
    args.pair("-map", format!("0:v:{}", v.source_type_index));
    if let Some(filters) = video_filters(resolution, hdr, *deinterlace, cmd.profile, None) {
        args.pair("-vf", filters);
    }
    args.pair("-c:v", "ffv1")
        .pair("-level", "3")
        .pair("-g", "1")
        .pair("-slices", "4");
    if cmd.setup.threads > 0 {
        args.pair("-threads", cmd.setup.threads.to_string());
    }
    args.arg("-an").arg("-sn").arg("-dn");
    args.pair("-f", "matroska").arg(output.as_os_str());
    Some(args.into_vec())
}

/// Sample encode from a reference made by [`build_reference_args`]: no
/// filters, same encoder settings as the main encode.
pub fn build_reference_sample_args(
    cmd: &EncodeCommand<'_>,
    reference: &Path,
) -> Option<Vec<OsString>> {
    let enc = cpu_video_encode(cmd)?;
    let tonemapped = matches!(
        cmd.plan.video.as_ref().map(|v| &v.action),
        Some(VideoAction::Encode {
            hdr: HdrConversion::TonemapToSdr { .. },
            ..
        })
    );
    let mut args = Args::new();
    base_args(&mut args);
    if cmd.setup.threads > 0 {
        args.pair("-threads", cmd.setup.threads.to_string());
    }
    args.arg("-i").arg(reference.as_os_str());
    args.pair("-map", "0:v:0");
    video_encoder_args(&mut args, &enc);
    if tonemapped {
        args.pair("-color_primaries", "bt709")
            .pair("-color_trc", "bt709")
            .pair("-colorspace", "bt709")
            .pair("-color_range", "tv");
    }
    args.arg("-an").arg("-sn").arg("-dn");
    args.pair("-f", "matroska").arg(cmd.output.as_os_str());
    Some(args.into_vec())
}

pub fn build_sidecar_args(source: &Path, sub: &SubtitleOutput, output: &Path) -> Vec<OsString> {
    let mut args = Args::new();
    base_args(&mut args);
    args.arg("-i").arg(source.as_os_str());
    args.pair("-map", format!("0:s:{}", sub.source_type_index));
    let format = match &sub.action {
        SubtitleAction::Sidecar { format, .. } => *format,
        _ => SidecarFormat::Mks,
    };
    match format {
        SidecarFormat::Sup => {
            args.pair("-c:s", "copy").pair("-f", "sup");
        }
        SidecarFormat::Mks => {
            args.pair("-c:s", "copy");
            if let Some(lang) = &sub.language {
                args.pair("-metadata:s:s:0", format!("language={lang}"));
            }
            args.pair("-f", "matroska");
        }
        SidecarFormat::Srt => {
            args.pair("-c:s", "srt").pair("-f", "srt");
        }
    }
    args.arg(output.as_os_str());
    args.into_vec()
}

/// Sidecar file name next to the main output: `Movie.ger.forced.sup`.
pub fn sidecar_path(main_output: &Path, sub: &SubtitleOutput) -> Option<std::path::PathBuf> {
    let SubtitleAction::Sidecar { format, suffix } = &sub.action else {
        return None;
    };
    let stem = main_output.file_stem()?.to_string_lossy();
    Some(main_output.with_file_name(format!("{stem}.{suffix}.{}", format.extension())))
}

#[cfg(test)]
mod tests;
