use super::*;
use crate::analyzer::resolution::CropRect;
use crate::encoding::plan::PlanWarning;
use crate::encoding::plan::{PlanCapabilities, build_plan};
use crate::media::fixtures::*;
use crate::media::model::*;

fn strings(args: &[OsString]) -> Vec<String> {
    args.iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

fn value_after(args: &[String], key: &str) -> Vec<String> {
    args.windows(2)
        .filter(|w| w[0] == key)
        .map(|w| w[1].clone())
        .collect()
}

fn uhd_hdr() -> MediaFile {
    let mut v = video("hevc", 3840, 2160);
    v.bit_depth = Some(10);
    v.pix_fmt = Some("yuv420p10le".into());
    v.hdr = HdrFormat::Hdr10;
    let mut eng = audio("truehd", None, 8, "eng");
    eng.title = Some("TrueHD 7.1".into());
    let mut ger_sub = subtitle("subrip", "ger");
    ger_sub.forced = true;
    let mut m = media(
        "mkv",
        50_000_000_000,
        7200.0,
        vec![v],
        vec![eng, audio("ac3", None, 6, "ger")],
        vec![ger_sub, subtitle("hdmv_pgs_subtitle", "eng")],
    );
    m.absolute_path = "/media/movies/Film; rm -rf $HOME `x`.mkv".into();
    m
}

#[test]
fn universal_hdr_command() {
    let m = uhd_hdr();
    let profile = EncodingProfile::universal();
    let plan = build_plan(&m, &profile, &PlanCapabilities::default());
    let out = Path::new("/work/1/out.mp4");
    let args = strings(&build_encode_args(&EncodeCommand {
        media: &m,
        plan: &plan,
        profile: &profile,
        setup: &EncoderSetup::cpu(),
        output: out,
        crf: None,
        window: None,
    }));

    // file name is passed verbatim as a single argument
    assert!(args.contains(&"/media/movies/Film; rm -rf $HOME `x`.mkv".to_string()));
    assert_eq!(value_after(&args, "-progress"), ["pipe:1"]);
    assert!(args.contains(&"-n".to_string()));
    assert_eq!(
        value_after(&args, "-map"),
        ["0:v:0", "0:a:0", "0:a:0", "0:a:1", "0:a:1", "0:s:0"],
        "video, eng converted + eng stereo, ger copy + ger stereo, text subtitle"
    );
    let vf = &value_after(&args, "-vf")[0];
    assert!(vf.starts_with("zscale=w=1920:h=1080"));
    assert!(vf.contains("tonemap=tonemap=hable"));
    assert!(vf.ends_with("format=yuv420p"));
    assert_eq!(value_after(&args, "-c:v"), ["libx264"]);
    assert_eq!(value_after(&args, "-crf"), ["20"]);
    assert_eq!(value_after(&args, "-color_trc"), ["bt709"]);
    // TrueHD → AC3 5.1
    assert_eq!(value_after(&args, "-c:a:0"), ["ac3"]);
    assert_eq!(value_after(&args, "-ac:a:0"), ["6"]);
    assert_eq!(value_after(&args, "-c:a:1"), ["aac"]);
    assert_eq!(value_after(&args, "-c:a:2"), ["copy"]);
    assert_eq!(value_after(&args, "-c:s:0"), ["mov_text"]);
    assert_eq!(value_after(&args, "-disposition:s:0"), ["forced"]);
    assert_eq!(value_after(&args, "-disposition:a:0"), ["default"]);
    assert_eq!(value_after(&args, "-movflags"), ["+faststart"]);
    assert_eq!(args.last().unwrap(), "/work/1/out.mp4");
}

#[test]
fn compat_stereo_per_language() {
    let m = uhd_hdr();
    let plan = build_plan(
        &m,
        &EncodingProfile::universal(),
        &PlanCapabilities::default(),
    );
    // eng: AC3 (converted) + AAC stereo; ger: AC3 copy + AAC stereo
    let langs: Vec<_> = plan
        .audio
        .iter()
        .map(|a| (a.language.clone().unwrap(), a.purpose))
        .collect();
    use crate::encoding::plan::AudioPurpose::*;
    assert_eq!(
        langs,
        [
            ("eng".to_string(), Converted),
            ("eng".to_string(), CompatibilityStereo),
            ("ger".to_string(), Original),
            ("ger".to_string(), CompatibilityStereo),
        ]
    );
}

#[test]
fn hardware_variants() {
    let m = media(
        "mkv",
        10_000_000_000,
        3600.0,
        vec![video("mpeg2video", 1920, 1080)],
        vec![audio("aac", Some("LC"), 2, "eng")],
        vec![],
    );
    let profile = EncodingProfile::universal();
    let plan = build_plan(&m, &profile, &PlanCapabilities::default());
    let run = |backend| {
        let setup = EncoderSetup {
            hw_encode: Some(backend),
            hw_decode: Some(backend),
            ..EncoderSetup::cpu()
        };
        strings(&build_encode_args(&EncodeCommand {
            media: &m,
            plan: &plan,
            profile: &profile,
            setup: &setup,
            output: Path::new("/o.mp4"),
            crf: Some(22),
            window: None,
        }))
    };
    let nv = run(HwBackend::Nvidia);
    assert_eq!(value_after(&nv, "-c:v"), ["h264_nvenc"]);
    assert_eq!(value_after(&nv, "-hwaccel"), ["cuda"]);
    assert_eq!(value_after(&nv, "-cq"), ["23"]);
    let va = run(HwBackend::Vaapi);
    assert_eq!(value_after(&va, "-vaapi_device"), ["/dev/dri/renderD128"]);
    assert!(value_after(&va, "-vf")[0].ends_with("format=nv12,hwupload"));
    assert!(value_after(&va, "-pix_fmt").is_empty());
    let qsv = run(HwBackend::IntelQsv);
    assert_eq!(value_after(&qsv, "-global_quality"), ["22"]);
}

#[test]
fn copy_and_sample_window() {
    let m = media(
        "mkv",
        2_000_000_000,
        3600.0,
        vec![video("h264", 1920, 1080)],
        vec![audio("aac", Some("LC"), 2, "eng")],
        vec![],
    );
    let profile = EncodingProfile::universal();
    let plan = build_plan(&m, &profile, &PlanCapabilities::default());
    let args = strings(&build_encode_args(&EncodeCommand {
        media: &m,
        plan: &plan,
        profile: &profile,
        setup: &EncoderSetup::cpu(),
        output: Path::new("/o.mp4"),
        crf: None,
        window: None,
    }));
    assert_eq!(value_after(&args, "-c:v"), ["copy"]);
    assert!(value_after(&args, "-vf").is_empty());

    let mut p2 = profile.clone();
    p2.video.allow_copy = false;
    let plan = build_plan(&m, &p2, &PlanCapabilities::default());
    let args = strings(&build_encode_args(&EncodeCommand {
        media: &m,
        plan: &plan,
        profile: &p2,
        setup: &EncoderSetup::cpu(),
        output: Path::new("/s.mkv"),
        crf: Some(24),
        window: Some(Window {
            start_s: 100.0,
            duration_s: 10.0,
        }),
    }));
    assert_eq!(value_after(&args, "-ss"), ["100.000"]);
    assert_eq!(value_after(&args, "-crf"), ["24"]);
    assert!(args.contains(&"-an".to_string()));
    assert_eq!(value_after(&args, "-map"), ["0:v:0"]);
}

#[test]
fn h264_level_for_high_fps() {
    assert_eq!(h264_level(Dimensions::new(1920, 1080), 59.94, "4.1"), "4.2");
    assert_eq!(
        h264_level(Dimensions::new(1920, 1080), 23.976, "4.1"),
        "4.1"
    );
    assert_eq!(h264_level(Dimensions::new(1280, 720), 60.0, "4.1"), "4.1");
}

#[test]
fn sidecar_names_and_args() {
    let sub = SubtitleOutput {
        source_type_index: 3,
        codec: SubtitleCodec::Pgs,
        action: SubtitleAction::Sidecar {
            format: SidecarFormat::Sup,
            suffix: "ger.forced".into(),
        },
        language: Some("ger".into()),
        title: None,
        default: false,
        forced: true,
    };
    let p = sidecar_path(Path::new("/out/Movie (2020).mp4"), &sub).unwrap();
    assert_eq!(p, Path::new("/out/Movie (2020).ger.forced.sup"));
    let args = strings(&build_sidecar_args(Path::new("/in.mkv"), &sub, &p));
    assert_eq!(value_after(&args, "-map"), ["0:s:3"]);
    assert_eq!(value_after(&args, "-f"), ["sup"]);
}

#[test]
fn quality_reference_matches_encoder_input() {
    let m = uhd_hdr();
    let profile = EncodingProfile::universal();
    let plan = build_plan(&m, &profile, &PlanCapabilities::default());
    let cpu = EncoderSetup::cpu();
    let cmd = EncodeCommand {
        media: &m,
        plan: &plan,
        profile: &profile,
        setup: &cpu,
        output: Path::new("/w/sample.mkv"),
        crf: Some(25),
        window: None,
    };
    let window = Window {
        start_s: 600.0,
        duration_s: 20.0,
    };
    let reference =
        strings(&build_reference_args(&cmd, window, Path::new("/w/reference_0.mkv")).unwrap());
    let main = strings(&build_encode_args(&EncodeCommand {
        output: Path::new("/w/out.mp4"),
        crf: None,
        ..cmd
    }));
    // identical filter chain (scale + tonemap + format) as the real encode
    assert_eq!(value_after(&reference, "-vf"), value_after(&main, "-vf"));
    assert_eq!(value_after(&reference, "-c:v"), ["ffv1"]);
    assert_eq!(value_after(&reference, "-ss"), ["600.000"]);
    assert!(reference.contains(&"/media/movies/Film; rm -rf $HOME `x`.mkv".to_string()));
    assert!(reference.contains(&"-n".to_string()));

    let sample =
        strings(&build_reference_sample_args(&cmd, Path::new("/w/reference_0.mkv")).unwrap());
    assert!(value_after(&sample, "-vf").is_empty(), "already filtered");
    assert_eq!(value_after(&sample, "-i"), ["/w/reference_0.mkv"]);
    assert_eq!(value_after(&sample, "-crf"), ["25"]);
    assert_eq!(value_after(&sample, "-c:v"), value_after(&main, "-c:v"));
    assert_eq!(
        value_after(&sample, "-preset"),
        value_after(&main, "-preset")
    );
    assert_eq!(value_after(&sample, "-color_trc"), ["bt709"]);
    assert_eq!(sample.last().map(String::as_str), Some("/w/sample.mkv"));

    // hardware encoders add their own filters: no reference path
    let mut hw = EncoderSetup::cpu();
    hw.hw_encode = Some(HwBackend::Vaapi);
    let hw_cmd = EncodeCommand { setup: &hw, ..cmd };
    assert!(build_reference_args(&hw_cmd, window, Path::new("/r.mkv")).is_none());
    assert!(build_reference_sample_args(&hw_cmd, Path::new("/r.mkv")).is_none());
}

#[test]
fn hevc_cpu_is_tagged_and_throttled() {
    let m = media(
        "mkv",
        10_000_000_000,
        3600.0,
        vec![video("h264", 1920, 1080)],
        vec![audio("aac", Some("LC"), 2, "eng")],
        vec![],
    );
    let mut profile = EncodingProfile::universal();
    profile.video.codec = TargetVideoCodec::Hevc;
    profile.video.preset = "medium".into();
    profile.video.allow_copy = false;
    let plan = build_plan(&m, &profile, &PlanCapabilities::default());
    let setup = EncoderSetup {
        threads: 4,
        ..EncoderSetup::cpu()
    };
    let args = strings(&build_encode_args(&EncodeCommand {
        media: &m,
        plan: &plan,
        profile: &profile,
        setup: &setup,
        output: Path::new("/o.mp4"),
        crf: Some(24),
        window: None,
    }));
    assert_eq!(value_after(&args, "-c:v"), ["libx265"]);
    assert_eq!(value_after(&args, "-tag:v"), ["hvc1"]);
    assert_eq!(value_after(&args, "-crf"), ["24"]);
    assert_eq!(value_after(&args, "-preset"), ["medium"]);
    assert_eq!(
        value_after(&args, "-x265-params"),
        ["log-level=error:pools=4"]
    );
    assert!(value_after(&args, "-profile:v").is_empty());
}

fn hevc_preserve_profile() -> EncodingProfile {
    let mut p = EncodingProfile::universal();
    p.video.codec = TargetVideoCodec::Hevc;
    p.hdr.strategy = crate::encoding::profile::HdrStrategy::Preserve;
    p
}

fn encode_args(m: &MediaFile, plan: &EncodePlan, profile: &EncodingProfile) -> Vec<String> {
    strings(&build_encode_args(&EncodeCommand {
        media: m,
        plan,
        profile,
        setup: &EncoderSetup::cpu(),
        output: Path::new("/work/1/out.mp4"),
        crf: None,
        window: None,
    }))
}

fn x265_params(args: &[String]) -> String {
    value_after(args, "-x265-params").remove(0)
}

#[test]
fn hdr10_is_kept_as_10_bit_with_mastering_metadata() {
    // UHD must be downscaled → re-encode keeps HDR10.
    let mut m = uhd_hdr();
    m.video[0].hdr_metadata = Some(HdrMetadata {
        mastering: Some(MasteringDisplay {
            red: (34000, 16000),
            green: (13250, 34500),
            blue: (7500, 3000),
            white_point: (15635, 16450),
            max_luminance: 10_000_000,
            min_luminance: 50,
        }),
        max_cll: Some((1000, 400)),
    });
    let profile = hevc_preserve_profile();
    let plan = build_plan(&m, &profile, &PlanCapabilities::default());
    let Some(VideoAction::Encode { hdr, ten_bit, .. }) = plan.video.as_ref().map(|v| &v.action)
    else {
        panic!("expected encode");
    };
    assert!(hdr.is_preserve() && *ten_bit);
    assert!(
        !plan
            .warnings
            .iter()
            .any(|w| matches!(w, PlanWarning::HdrToneMapped { .. }))
    );

    let args = encode_args(&m, &plan, &profile);
    let vf = value_after(&args, "-vf").remove(0);
    assert!(!vf.contains("tonemap") && !vf.contains("zscale"), "{vf}");
    assert!(vf.ends_with("format=yuv420p10le"), "{vf}");
    assert_eq!(value_after(&args, "-pix_fmt"), ["yuv420p10le"]);
    assert_eq!(value_after(&args, "-color_primaries"), ["bt2020"]);
    assert_eq!(value_after(&args, "-color_trc"), ["smpte2084"]);
    assert_eq!(value_after(&args, "-colorspace"), ["bt2020nc"]);
    let x265 = x265_params(&args);
    for part in [
        "colorprim=bt2020",
        "transfer=smpte2084",
        "colormatrix=bt2020nc",
        "hdr10=1",
        "master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,50)",
        "max-cll=1000,400",
    ] {
        assert!(
            x265.split(':').any(|p| p == part),
            "{part} missing in {x265}"
        );
    }
}

#[test]
fn hlg_and_dolby_vision_bases_are_kept() {
    let profile = hevc_preserve_profile();
    let mut m = uhd_hdr();
    m.video[0].hdr = HdrFormat::Hlg;
    let plan = build_plan(&m, &profile, &PlanCapabilities::default());
    let x265 = x265_params(&encode_args(&m, &plan, &profile));
    assert!(x265.contains("transfer=arib-std-b67") && !x265.contains("hdr10"));

    // Dolby Vision 8.1: kept as its HDR10 base, with a note.
    m.video[0].hdr = HdrFormat::DolbyVision(DolbyVisionInfo {
        profile: Some(8),
        compatibility: DvCompatibility::Hdr10,
        ..DolbyVisionInfo::default()
    });
    let plan = build_plan(&m, &profile, &PlanCapabilities::default());
    assert!(plan.warnings.contains(&PlanWarning::HdrKeptAsBase {
        format: m.video[0].hdr.label(),
        base: "HDR10".into(),
    }));
    let mut setup = EncoderSetup::cpu();
    setup.x265_dolbyvision_opt = true;
    let args = strings(&build_encode_args(&EncodeCommand {
        media: &m,
        plan: &plan,
        profile: &profile,
        setup: &setup,
        output: Path::new("/work/1/out.mp4"),
        crf: None,
        window: None,
    }));
    assert_eq!(value_after(&args, "-dolbyvision"), ["0"]);

    // Profile 5 has no base layer: not preservable → tone mapping (blocked).
    m.video[0].hdr = HdrFormat::DolbyVision(DolbyVisionInfo {
        profile: Some(5),
        compatibility: DvCompatibility::None,
        ..DolbyVisionInfo::default()
    });
    let plan = build_plan(&m, &profile, &PlanCapabilities::default());
    assert!(
        plan.warnings
            .iter()
            .any(|w| matches!(w, PlanWarning::HdrFormatNotPreservable { .. }))
    );
}

#[test]
fn hdr_falls_back_to_sdr_without_10_bit_x265() {
    let m = uhd_hdr();
    let profile = hevc_preserve_profile();
    let caps = PlanCapabilities {
        libx265_10bit: false,
        ..PlanCapabilities::default()
    };
    let plan = build_plan(&m, &profile, &caps);
    let Some(VideoAction::Encode { hdr, ten_bit, .. }) = plan.video.as_ref().map(|v| &v.action)
    else {
        panic!("expected encode");
    };
    assert!(hdr.is_tonemap() && !*ten_bit);
    assert!(plan.warnings.contains(&PlanWarning::HdrPreserveNo10Bit));
}

#[test]
fn hdr_preserve_copies_a_compatible_stream() {
    let mut v = video("hevc", 1920, 1080);
    v.bit_depth = Some(10);
    v.pix_fmt = Some("yuv420p10le".into());
    v.hdr = HdrFormat::Hdr10;
    // ~4.4 Mbit/s: below the copy threshold
    let m = media("mkv", 4_000_000_000, 7200.0, vec![v], vec![], vec![]);
    let profile = hevc_preserve_profile();
    let plan = build_plan(&m, &profile, &PlanCapabilities::default());
    assert!(plan.video_is_copied());
    assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
}

#[test]
fn sdr_hevc_is_10_bit_on_the_cpu_only() {
    let m = media(
        "mkv",
        20_000_000_000,
        7200.0,
        vec![video("mpeg2video", 1920, 1080)],
        vec![],
        vec![],
    );
    let mut profile = EncodingProfile::universal();
    profile.video.codec = TargetVideoCodec::Hevc;
    let plan = build_plan(&m, &profile, &PlanCapabilities::default());
    let args = encode_args(&m, &plan, &profile);
    assert_eq!(value_after(&args, "-pix_fmt"), ["yuv420p10le"]);
    // SDR: no colour tags, no HDR parameters.
    assert!(value_after(&args, "-color_trc").is_empty());
    assert!(!x265_params(&args).contains("colorprim"));

    // Hardware encoders stay at 8 bit.
    let nvenc = EncoderSetup {
        hw_encode: Some(HwBackend::Nvidia),
        ..EncoderSetup::cpu()
    };
    let args = strings(&build_encode_args(&EncodeCommand {
        media: &m,
        plan: &plan,
        profile: &profile,
        setup: &nvenc,
        output: Path::new("/work/1/out.mp4"),
        crf: None,
        window: None,
    }));
    assert_eq!(value_after(&args, "-pix_fmt"), ["yuv420p"]);

    // Profile switch off → 8 bit; H.264 never 10 bit.
    profile.video.hevc_10bit = false;
    let plan = build_plan(&m, &profile, &PlanCapabilities::default());
    assert_eq!(
        value_after(&encode_args(&m, &plan, &profile), "-pix_fmt"),
        ["yuv420p"]
    );
    let h264 = EncodingProfile::universal();
    let plan = build_plan(&m, &h264, &PlanCapabilities::default());
    assert_eq!(
        value_after(&encode_args(&m, &plan, &h264), "-pix_fmt"),
        ["yuv420p"]
    );
}

fn letterboxed(width: u32, height: u32, crop: CropRect) -> MediaFile {
    let mut v = video("mpeg2video", width, height);
    v.crop = Some(crop);
    media("mkv", 20_000_000_000, 7200.0, vec![v], vec![], vec![])
}

#[test]
fn black_bars_are_cropped_before_scaling() {
    let crop = CropRect {
        width: 3840,
        height: 1600,
        x: 0,
        y: 280,
    };
    let m = letterboxed(3840, 2160, crop);
    let profile = EncodingProfile::universal();
    let plan = build_plan(&m, &profile, &PlanCapabilities::default());
    let vp = plan.video.as_ref().unwrap();
    assert_eq!(vp.output_dimensions(), Dimensions::new(1920, 800));
    assert!(
        plan.warnings
            .contains(&crate::encoding::plan::PlanWarning::BlackBarsCropped {
                from: Dimensions::new(3840, 2160),
                to: Dimensions::new(3840, 1600),
            })
    );
    let vf = value_after(&encode_args(&m, &plan, &profile), "-vf").remove(0);
    assert!(
        vf.starts_with("crop=3840:1600:0:280,scale=1920:800:"),
        "{vf}"
    );

    // The quality reference sees the same picture as the encoder.
    let reference = strings(
        &build_reference_args(
            &EncodeCommand {
                media: &m,
                plan: &plan,
                profile: &profile,
                setup: &EncoderSetup::cpu(),
                output: Path::new("/work/1/s.mkv"),
                crf: None,
                window: None,
            },
            Window {
                start_s: 10.0,
                duration_s: 5.0,
            },
            Path::new("/work/1/ref.mkv"),
        )
        .unwrap(),
    );
    assert_eq!(value_after(&reference, "-vf"), [vf]);
}

#[test]
fn crop_is_off_when_disabled_or_stream_copied() {
    let crop = CropRect {
        width: 1920,
        height: 800,
        x: 0,
        y: 140,
    };
    let m = letterboxed(1920, 1080, crop);
    let mut profile = EncodingProfile::universal();
    profile.video.auto_crop = false;
    let plan = build_plan(&m, &profile, &PlanCapabilities::default());
    assert_eq!(
        plan.video.as_ref().unwrap().output_dimensions(),
        Dimensions::new(1920, 1080)
    );
    assert!(!value_after(&encode_args(&m, &plan, &profile), "-vf")[0].contains("crop="));

    // Bars alone are no reason to re-encode a compatible stream.
    let mut v = video("h264", 1920, 1080);
    v.crop = Some(crop);
    let m = media("mkv", 4_000_000_000, 7200.0, vec![v], vec![], vec![]);
    let plan = build_plan(
        &m,
        &EncodingProfile::universal(),
        &PlanCapabilities::default(),
    );
    assert!(plan.video_is_copied());
    assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
}
