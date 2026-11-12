//! End-to-end tests with real ffmpeg/ffprobe.
//!
//! Small test videos are generated on the fly. The tests are skipped (with a
//! message) when ffmpeg is not installed. No GPU is required: hardware
//! encoding is disabled and the CPU path is exercised.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use framewise::analyzer::recommend::{self, Recommendation};
use framewise::core::config::{EncodingSettings, ResourceSettings};
use framewise::core::tools::ToolPaths;
use framewise::encoding::hardware::{self, HardwareCaps};
use framewise::encoding::plan::{AudioPurpose, VideoAction};
use framewise::encoding::process::ProcessHandle;
use framewise::encoding::profile::{EncodingProfile, QualityMetric, TargetVideoCodec};
use framewise::encoding::quality::AutoChoice;
use framewise::media::model::{AudioCodec, HdrFormat, SubtitleCodec, VideoCodec};
use framewise::media::probe::{self, FileFacts};
use framewise::queue::job::{Job, JobState};
use framewise::queue::runner::{self, RunnerContext};
use framewise::scanner::{self, DiscoveryOptions, ScanRequest};

fn have_ffmpeg() -> bool {
    let ok = |bin: &str| {
        Command::new(bin)
            .arg("-version")
            .output()
            .is_ok_and(|o| o.status.success())
    };
    if ok("ffmpeg") && ok("ffprobe") {
        true
    } else {
        eprintln!("ffmpeg/ffprobe not installed – skipping integration test");
        false
    }
}

fn ffmpeg(args: &[&str]) {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .output()
        .expect("run ffmpeg");
    assert!(
        out.status.success(),
        "ffmpeg failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

const SRT: &str = "1\n00:00:00,500 --> 00:00:02,000\nHallo Welt\n\n2\n00:00:02,500 --> 00:00:03,500\nZweite Zeile\n";

/// MPEG-4 ASP video, AC3 5.1 (ger) + FLAC stereo (eng), SRT + ASS subtitles.
fn make_legacy_mkv(dir: &Path) -> PathBuf {
    let srt = dir.join("subs.srt");
    std::fs::write(&srt, SRT).unwrap();
    let out = dir.join("source/Movies/Legacy Film (2001).mkv");
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=s=640x360:r=25:d=4",
        "-f",
        "lavfi",
        "-t",
        "4",
        "-i",
        "sine=f=440:r=48000",
        "-f",
        "lavfi",
        "-t",
        "4",
        "-i",
        "sine=f=660:r=48000",
        "-i",
        srt.to_str().unwrap(),
        "-i",
        srt.to_str().unwrap(),
        "-map",
        "0:v",
        "-map",
        "1:a",
        "-map",
        "2:a",
        "-map",
        "3:s",
        "-map",
        "4:s",
        "-c:v",
        "mpeg4",
        "-q:v",
        "4",
        "-c:a:0",
        "ac3",
        "-ac:a:0",
        "6",
        "-b:a:0",
        "384k",
        "-c:a:1",
        "flac",
        "-ac:a:1",
        "2",
        "-c:s:0",
        "srt",
        "-c:s:1",
        "ass",
        "-metadata:s:a:0",
        "language=ger",
        "-metadata:s:a:0",
        "title=Deutsch 5.1",
        "-metadata:s:a:1",
        "language=eng",
        "-metadata:s:s:0",
        "language=ger",
        "-metadata:s:s:1",
        "language=eng",
        "-disposition:a:0",
        "default",
        "-disposition:s:0",
        "forced",
        out.to_str().unwrap(),
    ]);
    out
}

/// 10-bit HEVC tagged as HDR10 (PQ / BT.2020).
fn make_hdr_mkv(dir: &Path) -> PathBuf {
    let out = dir.join("source/UHD/HDR Clip.MKV");
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=s=640x360:r=24:d=3",
        "-f",
        "lavfi",
        "-t",
        "3",
        "-i",
        "sine=f=500:r=48000",
        "-map",
        "0:v",
        "-map",
        "1:a",
        "-c:v",
        "libx265",
        "-preset",
        "ultrafast",
        "-pix_fmt",
        "yuv420p10le",
        "-x265-params",
        "log-level=error:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc",
        "-color_primaries",
        "bt2020",
        "-color_trc",
        "smpte2084",
        "-colorspace",
        "bt2020nc",
        "-c:a",
        "aac",
        "-ac",
        "2",
        "-metadata:s:a:0",
        "language=eng",
        out.to_str().unwrap(),
    ]);
    out
}

fn file_hash(path: &Path) -> u64 {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut h = DefaultHasher::new();
    std::fs::read(path).unwrap().hash(&mut h);
    h.finish()
}

fn fast_profile() -> EncodingProfile {
    let mut p = EncodingProfile::universal();
    p.video.preset = "ultrafast".into();
    p
}

fn context(work: &Path, caps: HardwareCaps, profile: EncodingProfile) -> RunnerContext {
    RunnerContext {
        tools: ToolPaths {
            ffmpeg: "ffmpeg".into(),
            ffprobe: "ffprobe".into(),
        },
        work_root: work.join("work"),
        job_log_dir: work.join("logs"),
        caps,
        resources: ResourceSettings {
            hw_encode_enabled: false,
            hw_decode_enabled: false,
            ..ResourceSettings::default()
        },
        encoding: EncodingSettings {
            full_decode_test: true,
            ..EncodingSettings::default()
        },
        profile,
    }
}

async fn scan(root: &Path) -> scanner::ScanOutcome {
    scanner::run_scan(
        ScanRequest {
            discovery: DiscoveryOptions {
                root: root.to_path_buf(),
                extensions: vec!["mkv".into(), "mp4".into()],
                follow_symlinks: true,
                skip_hidden: true,
                exclude: vec![],
            },
            ffprobe: "ffprobe".into(),
            parallelism: 2,
            detect_duplicates: true,
        },
        Default::default(),
        Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn scan_encode_validate_legacy_file() {
    if !have_ffmpeg() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let source_file = make_legacy_mkv(dir.path());
    let before = file_hash(&source_file);

    // --- scan & analysis
    let outcome = scan(&dir.path().join("source")).await;
    assert!(outcome.issues.is_empty(), "{:?}", outcome.issues);
    assert_eq!(outcome.files.len(), 1);
    let media = &outcome.files[0];
    assert_eq!(
        media.relative_path,
        Path::new("Movies/Legacy Film (2001).mkv")
    );
    let v = media.primary_video().unwrap();
    assert_eq!(v.codec, VideoCodec::Mpeg4);
    assert_eq!((v.width, v.height), (640, 360));
    assert_eq!(media.audio.len(), 2);
    assert_eq!(media.audio[0].codec, AudioCodec::Ac3);
    assert_eq!(media.audio[0].channels, Some(6));
    assert_eq!(media.audio[0].language.as_deref(), Some("ger"));
    assert_eq!(media.audio[0].title.as_deref(), Some("Deutsch 5.1"));
    assert!(media.audio[0].default);
    assert_eq!(media.audio[1].codec, AudioCodec::Flac);
    assert_eq!(media.subtitles.len(), 2);
    assert_eq!(media.subtitles[0].codec, SubtitleCodec::Subrip);
    assert!(media.subtitles[0].forced);
    assert_eq!(media.subtitles[1].codec, SubtitleCodec::Ass);
    assert!((media.duration_s().unwrap() - 4.0).abs() < 0.5);

    let caps = hardware::scan(Path::new("ffmpeg"), "/dev/dri/renderD128").await;
    let profile = fast_profile();
    let analysis = recommend::analyze(media, &profile, &caps.plan_capabilities());
    assert_eq!(analysis.recommendation, Recommendation::Transcode);
    // ger: AC3 copy + AAC stereo; eng: FLAC→AAC stereo (already the compat track)
    let purposes: Vec<_> = analysis.plan.audio.iter().map(|a| a.purpose).collect();
    assert_eq!(
        purposes,
        [
            AudioPurpose::Original,
            AudioPurpose::CompatibilityStereo,
            AudioPurpose::Converted
        ]
    );

    // --- encode
    let target = dir.path().join("target/Movies/Legacy Film (2001).mp4");
    let job = Job::new(
        1,
        media.clone(),
        target.clone(),
        profile.id.clone(),
        analysis.plan.clone(),
    );
    let ctx = context(dir.path(), caps.clone(), profile.clone());
    let handle = ProcessHandle::new();
    let result = runner::run_job(&ctx, &job, &handle, |_| {}).await;
    assert_eq!(
        result.state,
        Some(JobState::Done),
        "error: {:?}",
        result.error
    );
    let report = result.validation.unwrap();
    assert!(report.passed(), "{report:#?}");
    assert!(target.exists());
    assert_eq!(file_hash(&source_file), before, "source must be untouched");
    assert!(
        !runner::job_work_dir(&ctx.work_root, 1).exists(),
        "work dir cleaned"
    );

    // --- check output streams
    let facts = FileFacts {
        absolute_path: target.clone(),
        relative_path: "x.mp4".into(),
        size_bytes: 0,
        modified_unix: 0,
    };
    let out = probe::analyze_file(Path::new("ffprobe"), &facts)
        .await
        .unwrap();
    let ov = out.primary_video().unwrap();
    assert_eq!(ov.codec, VideoCodec::H264);
    assert_eq!(ov.pix_fmt.as_deref(), Some("yuv420p"));
    assert_eq!((ov.width, ov.height), (640, 360), "no upscaling");
    let langs: Vec<_> = out
        .audio
        .iter()
        .map(|a| a.language.clone().unwrap())
        .collect();
    assert_eq!(langs, ["ger", "ger", "eng"]);
    assert_eq!(out.audio[0].codec, AudioCodec::Ac3);
    assert_eq!(out.audio[0].title.as_deref(), Some("Deutsch 5.1"));
    assert!(out.audio[0].default && !out.audio[1].default && !out.audio[2].default);
    assert_eq!(out.audio[1].codec, AudioCodec::Aac);
    assert_eq!(out.audio[1].channels, Some(2));
    assert_eq!(out.subtitles.len(), 2);
    assert!(
        out.subtitles
            .iter()
            .all(|s| s.codec == SubtitleCodec::MovText)
    );
    // MP4 cannot store "forced": title marker + .forced.srt sidecar instead
    assert_eq!(out.subtitles[0].title.as_deref(), Some("Forced"));
    let sidecar = dir
        .path()
        .join("target/Movies/Legacy Film (2001).ger.forced.srt");
    assert!(sidecar.exists(), "forced sidecar exported");
    assert!(
        std::fs::read_to_string(&sidecar)
            .unwrap()
            .contains("Hallo Welt")
    );

    // --- a second run must not overwrite the existing result
    let handle = ProcessHandle::new();
    let again = runner::run_job(
        &ctx,
        &Job {
            id: 2,
            ..job.clone()
        },
        &handle,
        |_| {},
    )
    .await;
    assert_eq!(again.state, Some(JobState::Skipped));
}

#[tokio::test(flavor = "multi_thread")]
async fn hdr_source_is_tonemapped_and_downscaled() {
    if !have_ffmpeg() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    make_hdr_mkv(dir.path());
    let outcome = scan(&dir.path().join("source")).await;
    assert_eq!(outcome.files.len(), 1);
    let media = &outcome.files[0];
    let v = media.primary_video().unwrap();
    assert_eq!(v.codec, VideoCodec::Hevc);
    assert_eq!(v.bit_depth, Some(10));
    assert_eq!(v.hdr, HdrFormat::Hdr10);

    let caps = hardware::scan(Path::new("ffmpeg"), "/dev/dri/renderD128").await;
    if !(caps.has_filter("zscale") && caps.has_filter("tonemap")) {
        eprintln!("ffmpeg without zscale – skipping tonemap test");
        return;
    }
    let mut profile = fast_profile();
    profile.video.max_width = 320;
    profile.video.max_height = 180;
    profile.quality.metric = QualityMetric::Ssim;
    profile.quality.sample_count = 1;
    profile.quality.sample_seconds = 2;
    let analysis = recommend::analyze(media, &profile, &caps.plan_capabilities());
    assert!(matches!(
        analysis.plan.video.as_ref().unwrap().action,
        VideoAction::Encode { .. }
    ));

    let target = dir.path().join("target/UHD/HDR Clip.mp4");
    let job = Job::new(
        7,
        media.clone(),
        target.clone(),
        profile.id.clone(),
        analysis.plan,
    );
    let ctx = context(dir.path(), caps, profile);
    let result = runner::run_job(&ctx, &job, &ProcessHandle::new(), |_| {}).await;
    assert_eq!(
        result.state,
        Some(JobState::Done),
        "error: {:?}",
        result.error
    );
    let q = result.quality.expect("quality measured");
    assert!(q.score > 0.5, "SSIM {q:?}");

    let facts = FileFacts {
        absolute_path: target,
        relative_path: "x.mp4".into(),
        size_bytes: 0,
        modified_unix: 0,
    };
    let out = probe::analyze_file(Path::new("ffprobe"), &facts)
        .await
        .unwrap();
    let ov = out.primary_video().unwrap();
    assert_eq!((ov.width, ov.height), (320, 180));
    assert_eq!(ov.hdr, HdrFormat::Sdr);
    assert_eq!(ov.color.transfer.as_deref(), Some("bt709"));
    assert_eq!(ov.bit_depth, Some(8));
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_removes_temporary_files() {
    if !have_ffmpeg() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    make_legacy_mkv(dir.path());
    let outcome = scan(&dir.path().join("source")).await;
    let media = &outcome.files[0];
    let mut profile = fast_profile();
    profile.video.preset = "placebo".into(); // slow enough to cancel
    let caps = HardwareCaps::default();
    let plan = framewise::encoding::plan::build_plan(media, &profile, &Default::default());
    let target = dir.path().join("target/x.mp4");
    let job = Job::new(3, media.clone(), target.clone(), profile.id.clone(), plan);
    let ctx = context(dir.path(), caps, profile);
    let handle = ProcessHandle::new();
    let h2 = handle.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        h2.cancel();
    });
    let result = runner::run_job(&ctx, &job, &handle, |_| {}).await;
    assert_eq!(
        result.state,
        Some(JobState::Cancelled),
        "{:?}",
        result.error
    );
    assert!(!target.exists());
    assert!(!runner::job_work_dir(&ctx.work_root, 3).exists());
}

/// Smart CRF on an HDR source: the search runs on pre-filtered lossless
/// references (source read and tonemapped once), keeps its trace, agrees
/// with the final measurement and leaves no temporary files behind.
#[tokio::test(flavor = "multi_thread")]
async fn smart_crf_uses_prefiltered_references() {
    if !have_ffmpeg() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    make_hdr_mkv(dir.path());
    let outcome = scan(&dir.path().join("source")).await;
    let media = &outcome.files[0];
    let caps = hardware::scan(Path::new("ffmpeg"), "/dev/dri/renderD128").await;
    if !(caps.has_filter("zscale") && caps.has_filter("tonemap")) {
        eprintln!("ffmpeg without zscale – skipping smart CRF test");
        return;
    }
    let mut profile = fast_profile();
    profile.video.max_width = 320;
    profile.video.max_height = 180;
    profile.video.crf = 23;
    profile.quality.metric = QualityMetric::Ssim;
    profile.quality.smart_crf = true;
    profile.quality.crf_min = 18;
    profile.quality.crf_max = 30;
    profile.quality.min_ssim = 0.95;
    profile.quality.sample_count = 1;
    profile.quality.sample_seconds = 2;
    let plan = framewise::encoding::plan::build_plan(media, &profile, &caps.plan_capabilities());
    let target = dir.path().join("target/UHD/HDR Clip.mp4");
    let job = Job::new(11, media.clone(), target.clone(), profile.id.clone(), plan);
    let ctx = context(dir.path(), caps, profile);
    let result = runner::run_job(&ctx, &job, &ProcessHandle::new(), |_| {}).await;
    assert_eq!(
        result.state,
        Some(JobState::Done),
        "error: {:?}",
        result.error
    );

    let q = result.quality.expect("quality measured");
    assert!(!q.search.is_empty(), "search trace kept after final check");
    assert!(q.search.len() <= 6, "{:?}", q.search);
    let at_chosen = q
        .search
        .iter()
        .find(|(crf, _)| *crf == q.crf)
        .map(|(_, s)| *s)
        .expect("chosen CRF was measured");
    assert!(at_chosen >= 0.95, "{:?}", q.search);
    // The final check pairs frames by number, so it agrees with the search
    // although source (MKV) and result (MP4) use different timestamp grids.
    assert!(
        (at_chosen - q.score).abs() < 0.01,
        "search {at_chosen} vs final {}",
        q.score
    );

    let log = std::fs::read_to_string(ctx.job_log_dir.join("job-11.log")).unwrap();
    assert!(log.contains("ffv1"), "lossless reference created");
    assert!(!runner::job_work_dir(&ctx.work_root, 11).exists());
    assert!(target.exists());
}

/// 60 s H.264 with a hard-to-compress noise section at 30–36 s.
fn make_scene_mkv(dir: &Path) -> PathBuf {
    let out = dir.join("source/Scenes/Scene Film.mkv");
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=s=320x180:r=25:d=60",
        "-vf",
        "noise=alls=80:allf=t:enable='between(t,30,36)'",
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-crf",
        "8",
        "-pix_fmt",
        "yuv420p",
        out.to_str().unwrap(),
    ]);
    out
}

#[tokio::test]
async fn auto_profile_picks_scenes_and_method() {
    if !have_ffmpeg() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    make_scene_mkv(dir.path());
    let outcome = scan(&dir.path().join("source")).await;
    let media = &outcome.files[0];
    let caps = hardware::scan(Path::new("ffmpeg"), "/dev/dri/renderD128").await;
    let mut profile = EncodingProfile::auto();
    profile.video.preset = "ultrafast".into();
    profile.video.allow_copy = false;
    profile.quality.hevc_preset = "ultrafast".into();
    profile.quality.metric = QualityMetric::Ssim;
    profile.quality.min_ssim = 0.95;
    profile.quality.sample_count = 4;
    profile.quality.sample_seconds = 3;
    let plan = framewise::encoding::plan::build_plan(media, &profile, &caps.plan_capabilities());
    let target = dir.path().join("target/Scenes/Scene Film.mp4");
    let job = Job::new(12, media.clone(), target.clone(), profile.id.clone(), plan);
    let ctx = context(dir.path(), caps.clone(), profile);
    let result = runner::run_job(&ctx, &job, &ProcessHandle::new(), |_| {}).await;
    assert_eq!(
        result.state,
        Some(JobState::Done),
        "error: {:?}",
        result.error
    );

    let q = result.quality.expect("quality measured");
    assert!(q.judge_min);
    let auto = q.auto.as_ref().expect("decision recorded");
    eprintln!(
        "auto decision: {auto:#?}\nfinal: {} / {}",
        q.score, q.min_score
    );
    assert!(auto.scene_windows, "notes: {:?}", auto.notes);
    assert_eq!(auto.window_starts.len(), 4);
    assert!(
        auto.window_starts
            .iter()
            .any(|s| *s + 3.0 > 30.0 && *s < 36.0),
        "noise section sampled: {:?}",
        auto.window_starts
    );
    let expected_candidates = if caps.plan_capabilities().libx265 {
        2
    } else {
        1
    };
    assert_eq!(auto.candidates.len(), expected_candidates, "{auto:?}");
    assert!(auto.candidates.iter().all(|c| c.size_ratio.is_some()));

    let probe = probe::probe(Path::new("ffprobe"), &target).await.unwrap();
    let out = probe::to_media_file(
        &probe,
        &FileFacts {
            absolute_path: target.clone(),
            relative_path: "x.mp4".into(),
            size_bytes: 1,
            modified_unix: 0,
        },
        false,
    );
    let codec = &out.primary_video().unwrap().codec;
    match auto.choice {
        AutoChoice::Encode(TargetVideoCodec::Hevc) => assert_eq!(*codec, VideoCodec::Hevc),
        AutoChoice::Encode(TargetVideoCodec::H264) | AutoChoice::CopyVideo => {
            assert_eq!(*codec, VideoCodec::H264)
        }
    }
    if matches!(auto.choice, AutoChoice::Encode(_)) {
        // Final check on the same windows agrees with the search.
        let at_chosen = q.search.iter().find(|(c, _)| *c == q.crf).unwrap().1;
        assert!(
            (at_chosen - q.min_score).abs() < 0.01,
            "search {at_chosen} vs final {}",
            q.min_score
        );
    }
    assert!(!runner::job_work_dir(&ctx.work_root, 12).exists());
}

/// 20 s MPEG-4 clip, 640×360 frame with a 640×264 picture (letterbox).
fn make_letterboxed_mkv(dir: &Path) -> PathBuf {
    let out = dir.join("source/Letterbox.mkv");
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=640x264:rate=25:duration=20,pad=640:360:0:48:black",
        "-c:v",
        "mpeg4",
        "-q:v",
        "3",
        "-g",
        "25",
        out.to_str().unwrap(),
    ]);
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn black_bars_are_detected_and_cropped() {
    if !have_ffmpeg() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    make_letterboxed_mkv(dir.path());
    let outcome = scan(&dir.path().join("source")).await;
    let media = &outcome.files[0];
    assert_eq!(
        media.primary_video().unwrap().crop,
        None,
        "not part of the scan"
    );

    let caps = hardware::scan(Path::new("ffmpeg"), "/dev/dri/renderD128").await;
    let profile = fast_profile();
    let analysis = recommend::analyze(media, &profile, &caps.plan_capabilities());
    let target = dir.path().join("target/Letterbox.mp4");
    let job = Job::new(
        1,
        media.clone(),
        target.clone(),
        profile.id.clone(),
        analysis.plan.clone(),
    );
    let ctx = context(dir.path(), caps, profile);
    let result = runner::run_job(&ctx, &job, &ProcessHandle::new(), |_| {}).await;
    assert_eq!(result.state, Some(JobState::Done), "{:?}", result.error);
    assert!(result.validation.unwrap().passed());

    let facts = FileFacts {
        absolute_path: target.clone(),
        relative_path: "x.mp4".into(),
        size_bytes: 0,
        modified_unix: 0,
    };
    let out = probe::analyze_file(Path::new("ffprobe"), &facts)
        .await
        .unwrap();
    let v = out.primary_video().unwrap();
    assert_eq!((v.width, v.height), (640, 264));
}
