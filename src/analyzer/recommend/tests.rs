use super::*;
use crate::encoding::plan::*;
use crate::encoding::profile::*;
use crate::media::fixtures::*;
use crate::media::model::*;

const GB: u64 = 1_000_000_000;

fn caps() -> PlanCapabilities {
    PlanCapabilities::default()
}

#[test]
fn compatible_efficient_mp4_is_optimal() {
    let m = media(
        "mp4",
        2 * GB,
        5400.0,
        vec![video("h264", 1920, 1080)],
        vec![audio("aac", Some("LC"), 2, "ger")],
        vec![],
    );
    let a = analyze(&m, &EncodingProfile::universal(), &caps());
    assert_eq!(a.compatibility, Compatibility::Compatible);
    assert_eq!(a.recommendation, Recommendation::AlreadyOptimal);
    assert_eq!(a.potential_score, 0);
}

#[test]
fn compatible_mkv_is_remuxed() {
    let m = media(
        "mkv",
        2 * GB,
        5400.0,
        vec![video("h264", 1920, 1080)],
        vec![
            audio("ac3", None, 6, "ger"),
            audio("aac", Some("LC"), 2, "ger"),
        ],
        vec![subtitle("subrip", "ger")],
    );
    let a = analyze(&m, &EncodingProfile::universal(), &caps());
    assert_eq!(a.recommendation, Recommendation::Remux);
    assert!(a.plan.video_is_copied());
    // AAC stereo already exists → no extra track
    assert_eq!(a.plan.audio.len(), 2);
    assert_eq!(a.plan.mov_text_count(), 1);
}

#[test]
fn uhd_hevc_hdr_remux_is_transcoded_with_tonemapping() {
    let mut v = video("hevc", 3840, 2160);
    v.bit_depth = Some(10);
    v.pix_fmt = Some("yuv420p10le".into());
    v.color.transfer = Some("smpte2084".into());
    v.hdr = HdrFormat::Hdr10;
    let mut atmos = audio("truehd", Some("Dolby TrueHD + Dolby Atmos"), 8, "eng");
    atmos.default = true;
    let m = media(
        "mkv",
        60 * GB,
        7200.0,
        vec![v],
        vec![atmos, audio("dts", Some("DTS-HD MA"), 6, "ger")],
        vec![
            subtitle("hdmv_pgs_subtitle", "ger"),
            subtitle("subrip", "eng"),
        ],
    );
    let a = analyze(&m, &EncodingProfile::universal(), &caps());
    assert_eq!(a.recommendation, Recommendation::Transcode);
    assert_eq!(a.compatibility, Compatibility::Incompatible);
    let vp = a.plan.video.as_ref().unwrap();
    match &vp.action {
        VideoAction::Encode {
            resolution, hdr, ..
        } => {
            assert_eq!(resolution.output.width, 1920);
            assert!(matches!(hdr, HdrConversion::TonemapToSdr { .. }));
        }
        VideoAction::Copy => panic!("must encode"),
    }
    // Both originals converted (AC3, max 5.1) + one AAC stereo per language.
    assert_eq!(a.plan.audio.len(), 4);
    let w = &a.plan.warnings;
    assert!(w.contains(&PlanWarning::AtmosLost { track: 1 }));
    assert!(w.contains(&PlanWarning::ChannelsReduced {
        track: 1,
        from: 8,
        to: 6
    }));
    assert!(
        w.iter()
            .any(|w| matches!(w, PlanWarning::BitmapSubtitleSidecar { .. }))
    );
    assert!(a.plan.audio.iter().filter(|o| o.default).count() == 1);
    assert!(a.plan.audio[0].default, "original default stays default");
    assert!(a.estimated_savings.unwrap() > 40 * GB);
    assert!(a.potential_score > 70);
}

#[test]
fn no_audio_track_is_lost() {
    let m = media(
        "mkv",
        10 * GB,
        3600.0,
        vec![video("h264", 1920, 1080)],
        vec![
            audio("flac", None, 2, "jpn"),
            audio("opus", None, 6, "eng"),
            audio("pcm_s16le", None, 2, "ger"),
            audio("truehd", None, 8, "ger"),
        ],
        vec![],
    );
    for profile in EncodingProfile::builtins() {
        let a = analyze(&m, &profile, &caps());
        for src in &m.audio {
            assert!(
                a.plan
                    .audio
                    .iter()
                    .any(|o| o.source_type_index == src.type_index
                        && o.purpose != AudioPurpose::CompatibilityStereo),
                "track {} lost in profile {}",
                src.type_index,
                profile.id
            );
        }
    }
}

#[test]
fn preserve_original_keeps_dts() {
    let m = media(
        "mkv",
        10 * GB,
        3600.0,
        vec![video("h264", 1920, 1080)],
        vec![audio("dts", Some("DTS-HD MA"), 6, "eng")],
        vec![],
    );
    let mut p = EncodingProfile::universal();
    p.audio.strategy = AudioStrategy::PreserveOriginal;
    let a = analyze(&m, &p, &caps());
    assert_eq!(a.plan.audio[0].action, AudioAction::Copy);
    assert!(
        a.plan
            .warnings
            .iter()
            .any(|w| matches!(w, PlanWarning::LimitedClientSupport { .. }))
    );
    // plus AAC stereo compatibility track
    assert_eq!(a.plan.audio[1].purpose, AudioPurpose::CompatibilityStereo);
}

#[test]
fn subtitles_are_never_silently_dropped() {
    let mut forced = subtitle("hdmv_pgs_subtitle", "ger");
    forced.forced = true;
    let m = media(
        "mkv",
        GB,
        3600.0,
        vec![video("h264", 1280, 720)],
        vec![audio("aac", Some("LC"), 2, "ger")],
        vec![
            forced,
            subtitle("hdmv_pgs_subtitle", "ger"),
            subtitle("dvd_subtitle", "eng"),
            subtitle("ass", "eng"),
            subtitle("eia_608", "eng"),
        ],
    );
    let mut p = EncodingProfile::universal();
    let a = analyze(&m, &p, &caps());
    assert_eq!(a.plan.subtitles.len(), 5);
    let suffixes: Vec<_> = a
        .plan
        .sidecars()
        .map(|s| match &s.action {
            SubtitleAction::Sidecar { suffix, format } => {
                format!("{suffix}.{}", format.extension())
            }
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(suffixes, ["ger.forced.sup", "ger.sup", "eng.mks"]);
    assert!(
        a.plan
            .warnings
            .contains(&PlanWarning::AssStylingLost { track: 4 })
    );
    assert!(
        a.plan
            .warnings
            .iter()
            .any(|w| matches!(w, PlanWarning::UnsupportedSubtitleSkipped { track: 5, .. }))
    );

    p.subtitles.bitmap = BitmapSubtitleAction::Skip;
    let a = analyze(&m, &p, &caps());
    let skipped = a
        .plan
        .warnings
        .iter()
        .filter(|w| matches!(w, PlanWarning::BitmapSubtitleSkipped { .. }))
        .count();
    assert_eq!(skipped, 3);
}

#[test]
fn dolby_vision_profile5_is_blocked() {
    let mut v = video("hevc", 3840, 2160);
    v.bit_depth = Some(10);
    v.hdr = HdrFormat::DolbyVision(DolbyVisionInfo {
        profile: Some(5),
        level: Some(6),
        compatibility: DvCompatibility::None,
        has_enhancement_layer: false,
    });
    let m = media(
        "mp4",
        10 * GB,
        3600.0,
        vec![v],
        vec![audio("eac3", None, 6, "eng")],
        vec![],
    );
    let a = analyze(&m, &EncodingProfile::universal(), &caps());
    assert_eq!(a.recommendation, Recommendation::Review);
    assert!(a.plan.blockers.contains(&PlanBlocker::DolbyVisionProfile5));
}

#[test]
fn hdr_without_zscale_is_blocked() {
    let mut v = video("hevc", 3840, 2160);
    v.hdr = HdrFormat::Hlg;
    let m = media(
        "mkv",
        10 * GB,
        3600.0,
        vec![v],
        vec![audio("aac", None, 2, "eng")],
        vec![],
    );
    let caps = PlanCapabilities {
        zscale: false,
        ..PlanCapabilities::default()
    };
    let a = analyze(&m, &EncodingProfile::universal(), &caps);
    assert!(a.plan.blockers.contains(&PlanBlocker::TonemapUnavailable));
}

#[test]
fn bloated_h264_is_reencoded_and_space_saver_always_encodes() {
    let mut v = video("h264", 1920, 1080);
    v.bit_rate = Some(35_000_000);
    let m = media(
        "mp4",
        30 * GB,
        7200.0,
        vec![v],
        vec![audio("aac", Some("LC"), 2, "eng")],
        vec![],
    );
    let a = analyze(&m, &EncodingProfile::universal(), &caps());
    assert_eq!(a.recommendation, Recommendation::Transcode);
    assert_eq!(a.compatibility, Compatibility::Compatible);

    let small = media(
        "mp4",
        GB,
        3600.0,
        vec![video("h264", 1280, 720)],
        vec![audio("aac", Some("LC"), 2, "eng")],
        vec![],
    );
    let a = analyze(&small, &EncodingProfile::space_saver(), &caps());
    assert!(!a.plan.video_is_copied());
}

#[test]
fn estimates_are_never_negative() {
    // Already tiny file: estimate may exceed size, savings clamp to 0.
    let m = media(
        "mkv",
        50_000_000,
        3600.0,
        vec![video("mpeg4", 640, 360)],
        vec![audio("mp3", None, 2, "eng")],
        vec![],
    );
    let a = analyze(&m, &EncodingProfile::universal(), &caps());
    assert!(a.estimated_size.is_some());
    assert!(a.estimated_savings.unwrap() <= m.size_bytes);
}
