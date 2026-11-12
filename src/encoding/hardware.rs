//! Hardware capability scanner.
//!
//! Availability is never assumed: an encoder counts as usable only if ffmpeg
//! lists it **and** a one-frame test encode succeeds on this machine.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::plan::PlanCapabilities;
use super::profile::TargetVideoCodec;
use crate::core::config::HwBackend;
use crate::core::tools::run_capture;

/// Order used by `HwBackend::Auto`.
pub const AUTO_ORDER: [HwBackend; 5] = [
    HwBackend::Nvidia,
    HwBackend::IntelQsv,
    HwBackend::Vaapi,
    HwBackend::AmdAmf,
    HwBackend::VideoToolbox,
];

pub fn encoder_name(backend: HwBackend, codec: TargetVideoCodec) -> Option<&'static str> {
    let h264 = codec == TargetVideoCodec::H264;
    Some(match backend {
        HwBackend::Auto => return None,
        HwBackend::Cpu => {
            if h264 {
                "libx264"
            } else {
                "libx265"
            }
        }
        HwBackend::Nvidia => {
            if h264 {
                "h264_nvenc"
            } else {
                "hevc_nvenc"
            }
        }
        HwBackend::IntelQsv => {
            if h264 {
                "h264_qsv"
            } else {
                "hevc_qsv"
            }
        }
        HwBackend::Vaapi => {
            if h264 {
                "h264_vaapi"
            } else {
                "hevc_vaapi"
            }
        }
        HwBackend::AmdAmf => {
            if h264 {
                "h264_amf"
            } else {
                "hevc_amf"
            }
        }
        HwBackend::VideoToolbox => {
            if h264 {
                "h264_videotoolbox"
            } else {
                "hevc_videotoolbox"
            }
        }
    })
}

/// `-hwaccel` value for hardware decoding.
pub fn hwaccel_name(backend: HwBackend) -> Option<&'static str> {
    match backend {
        HwBackend::Nvidia => Some("cuda"),
        HwBackend::IntelQsv => Some("qsv"),
        HwBackend::Vaapi => Some("vaapi"),
        HwBackend::AmdAmf => Some(if cfg!(windows) { "d3d11va" } else { "vaapi" }),
        HwBackend::VideoToolbox => Some("videotoolbox"),
        HwBackend::Auto | HwBackend::Cpu => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncoderTest {
    pub backend: HwBackend,
    pub encoder: String,
    pub codec: TargetVideoCodec,
    pub works: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuInfo {
    pub name: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HardwareCaps {
    pub encoders: BTreeSet<String>,
    pub decoders: BTreeSet<String>,
    pub hwaccels: Vec<String>,
    pub filters: BTreeSet<String>,
    pub tests: Vec<EncoderTest>,
    pub gpus: Vec<GpuInfo>,
    pub render_devices: Vec<PathBuf>,
    /// libx265 accepts `yuv420p10le` (HEVC Main 10).
    #[serde(default)]
    pub libx265_10bit: bool,
    /// libx265 has the `-dolbyvision` option (ffmpeg ≥ 7; default "auto").
    #[serde(default)]
    pub libx265_dolbyvision: bool,
}

impl HardwareCaps {
    pub fn has_filter(&self, name: &str) -> bool {
        self.filters.contains(name)
    }

    pub fn plan_capabilities(&self) -> PlanCapabilities {
        PlanCapabilities {
            zscale: self.has_filter("zscale"),
            tonemap: self.has_filter("tonemap"),
            libx264: self.encoders.contains("libx264"),
            libx265: self.encoders.contains("libx265"),
            libx265_10bit: self.libx265_10bit,
        }
    }

    pub fn has_vmaf(&self) -> bool {
        self.has_filter("libvmaf")
    }

    pub fn encoder_works(&self, backend: HwBackend, codec: TargetVideoCodec) -> bool {
        self.tests
            .iter()
            .any(|t| t.backend == backend && t.codec == codec && t.works)
    }

    /// Resolve the user's choice to a concrete, verified backend.
    /// `None` means CPU.
    pub fn resolve_encoder(&self, choice: HwBackend, codec: TargetVideoCodec) -> Option<HwBackend> {
        match choice {
            HwBackend::Cpu => None,
            HwBackend::Auto => AUTO_ORDER
                .into_iter()
                .find(|b| self.encoder_works(*b, codec)),
            specific => self.encoder_works(specific, codec).then_some(specific),
        }
    }

    /// Resolve hardware decoding; only offered if ffmpeg lists the hwaccel.
    pub fn resolve_decoder(&self, choice: HwBackend) -> Option<HwBackend> {
        let available = |b: HwBackend| {
            hwaccel_name(b).is_some_and(|name| self.hwaccels.iter().any(|h| h == name))
                && (b != HwBackend::Nvidia || self.encoder_works(b, TargetVideoCodec::H264))
        };
        match choice {
            HwBackend::Cpu => None,
            HwBackend::Auto => AUTO_ORDER.into_iter().find(|b| available(*b)),
            specific => available(specific).then_some(specific),
        }
    }
}

/// Parse `ffmpeg -encoders` / `-decoders` / `-filters` listings: the name is
/// the second column of every line after the `------` separator.
pub fn parse_listing(text: &str) -> BTreeSet<String> {
    let mut started = false;
    let mut names = BTreeSet::new();
    for line in text.lines() {
        if !started {
            started = line.trim_start().starts_with("---");
            continue;
        }
        let mut cols = line.split_whitespace();
        if let (Some(_flags), Some(name)) = (cols.next(), cols.next()) {
            names.insert(name.to_string());
        }
    }
    // `-filters` has no separator line in some builds: fall back to all lines
    if !started {
        for line in text.lines() {
            let mut cols = line.split_whitespace();
            if let (Some(flags), Some(name)) = (cols.next(), cols.next())
                && flags.len() <= 3
                && flags.chars().all(|c| matches!(c, 'T' | 'S' | 'C' | '.'))
            {
                names.insert(name.to_string());
            }
        }
    }
    names
}

pub fn parse_hwaccels(text: &str) -> Vec<String> {
    text.lines()
        .skip_while(|l| !l.contains("Hardware acceleration methods"))
        .skip(1)
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

async fn listing(ffmpeg: &Path, flag: &str) -> String {
    match run_capture(
        "ffmpeg",
        ffmpeg,
        std::iter::once("-hide_banner").chain(flag.split_whitespace()),
        Duration::from_secs(15),
        flag,
    )
    .await
    {
        Ok(out) => String::from_utf8_lossy(&out.stdout).into_owned(),
        Err(err) => {
            tracing::warn!("ffmpeg {flag} failed: {err}");
            String::new()
        }
    }
}

/// libx265 details from `ffmpeg -h encoder=libx265`: (10 bit, -dolbyvision).
pub fn parse_x265_help(help: &str) -> (bool, bool) {
    let ten_bit = help
        .lines()
        .find(|l| l.trim_start().starts_with("Supported pixel formats:"))
        .is_some_and(|l| l.split_whitespace().any(|f| f == "yuv420p10le"));
    let dolbyvision = help
        .lines()
        .any(|l| l.trim_start().starts_with("-dolbyvision "));
    (ten_bit, dolbyvision)
}

/// Arguments for a one-frame functional test of an encoder.
pub fn test_encode_args(backend: HwBackend, encoder: &str, vaapi_device: &str) -> Vec<String> {
    let mut args: Vec<String> = ["-hide_banner", "-nostdin", "-loglevel", "error"]
        .map(String::from)
        .to_vec();
    if backend == HwBackend::Vaapi {
        args.extend(["-vaapi_device".into(), vaapi_device.into()]);
    }
    args.extend(
        [
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=256x256:r=25:d=0.2",
            "-frames:v",
            "2",
        ]
        .map(String::from),
    );
    match backend {
        HwBackend::Vaapi => args.extend(["-vf".into(), "format=nv12,hwupload".into()]),
        HwBackend::IntelQsv => args.extend(["-vf".into(), "format=nv12".into()]),
        _ => args.extend(["-pix_fmt".into(), "yuv420p".into()]),
    }
    args.extend([
        "-c:v".into(),
        encoder.into(),
        "-f".into(),
        "null".into(),
        "-".into(),
    ]);
    args
}

fn detect_render_devices() -> Vec<PathBuf> {
    let mut devices: Vec<PathBuf> = std::fs::read_dir("/dev/dri")
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with("renderD"))
                })
                .collect()
        })
        .unwrap_or_default();
    devices.sort();
    devices
}

/// Best-effort GPU model detection (nvidia-smi, Linux sysfs/lspci).
async fn detect_gpus() -> Vec<GpuInfo> {
    let mut gpus = Vec::new();
    if let Ok(out) = run_capture(
        "nvidia-smi",
        Path::new("nvidia-smi"),
        ["--query-gpu=name,driver_version", "--format=csv,noheader"],
        Duration::from_secs(5),
        "GPU query",
    )
    .await
    {
        for line in String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
        {
            gpus.push(GpuInfo {
                name: line.trim().to_string(),
                source: "nvidia-smi".into(),
            });
        }
    }
    if cfg!(target_os = "linux")
        && let Ok(out) = run_capture(
            "lspci",
            Path::new("lspci"),
            Vec::<&str>::new(),
            Duration::from_secs(5),
            "PCI listing",
        )
        .await
    {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let lower = line.to_lowercase();
            if (lower.contains("vga compatible")
                || lower.contains("3d controller")
                || lower.contains("display controller"))
                && let Some((_, name)) = line.split_once(": ")
                && !gpus.iter().any(|g| name.contains(&g.name))
            {
                gpus.push(GpuInfo {
                    name: name.trim().to_string(),
                    source: "lspci".into(),
                });
            }
        }
    }
    gpus
}

/// Run the full capability scan.
pub async fn scan(ffmpeg: &Path, vaapi_device: &str) -> HardwareCaps {
    let (encoders, decoders, hwaccels, filters, gpus) = tokio::join!(
        listing(ffmpeg, "-encoders"),
        listing(ffmpeg, "-decoders"),
        listing(ffmpeg, "-hwaccels"),
        listing(ffmpeg, "-filters"),
        detect_gpus(),
    );
    let mut caps = HardwareCaps {
        encoders: parse_listing(&encoders),
        decoders: parse_listing(&decoders),
        hwaccels: parse_hwaccels(&hwaccels),
        filters: parse_listing(&filters),
        gpus,
        render_devices: detect_render_devices(),
        tests: Vec::new(),
        libx265_10bit: false,
        libx265_dolbyvision: false,
    };
    if caps.encoders.contains("libx265") {
        let help = listing(ffmpeg, "-h encoder=libx265").await;
        (caps.libx265_10bit, caps.libx265_dolbyvision) = parse_x265_help(&help);
    }

    for backend in AUTO_ORDER {
        for codec in [TargetVideoCodec::H264, TargetVideoCodec::Hevc] {
            let Some(encoder) = encoder_name(backend, codec) else {
                continue;
            };
            if !caps.encoders.contains(encoder) {
                continue;
            }
            let args = test_encode_args(backend, encoder, vaapi_device);
            let result = run_capture(
                "ffmpeg",
                ffmpeg,
                &args,
                Duration::from_secs(20),
                "encoder test",
            )
            .await;
            caps.tests.push(EncoderTest {
                backend,
                encoder: encoder.into(),
                codec,
                works: result.is_ok(),
                error: result.err().map(|e| e.to_string()),
            });
        }
    }
    tracing::info!(
        "hardware scan: {} encoders, {} filters, working hw encoders: {:?}",
        caps.encoders.len(),
        caps.filters.len(),
        caps.tests
            .iter()
            .filter(|t| t.works)
            .map(|t| &t.encoder)
            .collect::<Vec<_>>()
    );
    caps
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENCODERS: &str = "Encoders:
 V..... = Video
 ------
 V....D libx264              libx264 H.264 / AVC
 V....D h264_nvenc           NVIDIA NVENC H.264 encoder
 A....D aac                  AAC (Advanced Audio Coding)
";

    const FILTERS: &str = "Filters:
  T.. = Timeline support
  | = Source or sink filter
 TS. psnr              VV->V      Calculate the PSNR between two video streams.
 .S. tonemap           V->V       Conversion to/from different dynamic ranges.
 ..C zscale            V->V       Apply resizing, colorspace and bit depth conversion.
";

    #[test]
    fn parses_listings() {
        let enc = parse_listing(ENCODERS);
        assert!(enc.contains("libx264") && enc.contains("h264_nvenc") && enc.contains("aac"));
        assert!(!enc.contains("V....."));
        let filters = parse_listing(FILTERS);
        assert!(
            filters.contains("zscale") && filters.contains("tonemap") && filters.contains("psnr")
        );
        let hw = parse_hwaccels("Hardware acceleration methods:\nvdpau\ncuda\nvaapi\n\n");
        assert_eq!(hw, ["vdpau", "cuda", "vaapi"]);
    }

    #[test]
    fn resolution_never_assumes_hardware() {
        let mut caps = HardwareCaps::default();
        caps.encoders.insert("h264_nvenc".into());
        caps.hwaccels.push("cuda".into());
        // listed but test failed → not used
        caps.tests.push(EncoderTest {
            backend: HwBackend::Nvidia,
            encoder: "h264_nvenc".into(),
            codec: TargetVideoCodec::H264,
            works: false,
            error: Some("no device".into()),
        });
        assert_eq!(
            caps.resolve_encoder(HwBackend::Auto, TargetVideoCodec::H264),
            None
        );
        assert_eq!(
            caps.resolve_encoder(HwBackend::Nvidia, TargetVideoCodec::H264),
            None
        );
        assert_eq!(caps.resolve_decoder(HwBackend::Nvidia), None);
        caps.tests[0].works = true;
        assert_eq!(
            caps.resolve_encoder(HwBackend::Auto, TargetVideoCodec::H264),
            Some(HwBackend::Nvidia)
        );
        assert_eq!(
            caps.resolve_encoder(HwBackend::Cpu, TargetVideoCodec::H264),
            None
        );
        assert_eq!(
            caps.resolve_decoder(HwBackend::Auto),
            Some(HwBackend::Nvidia)
        );
    }

    #[test]
    fn x265_help_reveals_10_bit_and_dolby_vision() {
        let old = "Encoder libx265 [libx265 H.265 / HEVC]:\n    Supported pixel formats: yuv420p yuvj420p yuv420p10le gray\nlibx265 AVOptions:\n  -crf               <float>      E..V.......\n";
        assert_eq!(parse_x265_help(old), (true, false));
        let new = "    Supported pixel formats: yuv420p yuv444p\n  -dolbyvision       <boolean>    E..V....... Enable Dolby Vision RPU coding (default auto)\n";
        assert_eq!(parse_x265_help(new), (false, true));
        assert_eq!(parse_x265_help(""), (false, false));
    }
}
