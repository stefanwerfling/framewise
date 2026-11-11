//! Result validation: a converted file only counts as successful if every
//! check passes. Comparison logic is pure (testable); I/O is in [`validate`].

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::plan::{EncodePlan, SubtitleAction, VideoAction};
use super::process::{ProcessHandle, RunOptions, run_ffmpeg};
use super::profile::TargetVideoCodec;
use crate::media::model::{MediaFile, VideoCodec};
use crate::media::probe::{self, FileFacts};
use crate::tr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckKind {
    OutputExists,
    OutputNotEmpty,
    ProbeReadable,
    VideoStream,
    VideoCodec,
    Resolution,
    AudioStreams,
    AudioLanguages,
    AudioTitles,
    SubtitleStreams,
    SubtitleLanguages,
    Duration,
    DecodeTest,
    Sidecar,
    Quality,
}

impl CheckKind {
    pub fn label(self) -> &'static str {
        match self {
            CheckKind::OutputExists => tr!("Output exists"),
            CheckKind::OutputNotEmpty => tr!("File size"),
            CheckKind::ProbeReadable => tr!("ffprobe readable"),
            CheckKind::VideoStream => tr!("Video stream"),
            CheckKind::VideoCodec => tr!("Video codec"),
            CheckKind::Resolution => tr!("Resolution"),
            CheckKind::AudioStreams => tr!("Audio streams"),
            CheckKind::AudioLanguages => tr!("Audio languages"),
            CheckKind::AudioTitles => tr!("Audio track titles"),
            CheckKind::SubtitleStreams => tr!("Subtitle streams"),
            CheckKind::SubtitleLanguages => tr!("Subtitle languages"),
            CheckKind::Duration => tr!("Duration"),
            CheckKind::DecodeTest => tr!("Full decode test"),
            CheckKind::Sidecar => tr!("Sidecar file"),
            CheckKind::Quality => tr!("Quality measurement"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationCheck {
    pub kind: CheckKind,
    pub passed: bool,
    /// Language-neutral values (numbers, codes, paths).
    pub expected: String,
    pub actual: String,
}

impl ValidationCheck {
    fn new(kind: CheckKind, passed: bool, expected: impl ToString, actual: impl ToString) -> Self {
        Self {
            kind,
            passed,
            expected: expected.to_string(),
            actual: actual.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ValidationReport {
    pub checks: Vec<ValidationCheck>,
}

impl ValidationReport {
    pub fn passed(&self) -> bool {
        !self.checks.is_empty() && self.checks.iter().all(|c| c.passed)
    }

    pub fn failures(&self) -> impl Iterator<Item = &ValidationCheck> {
        self.checks.iter().filter(|c| !c.passed)
    }

    /// Short, language-neutral-ish summary of failed checks.
    pub fn failure_summary(&self) -> String {
        self.failures()
            .map(|c| format!("{}: {} ≠ {}", c.kind.label(), c.expected, c.actual))
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// Allowed duration difference: max(1.5 s, 0.1 %).
pub fn duration_tolerance(source_s: f64) -> f64 {
    (source_s * 0.001).max(1.5)
}

fn lang(l: &Option<String>) -> String {
    l.clone().unwrap_or_else(|| "und".into())
}

/// Compare the probed output with the plan and the source.
pub fn compare(plan: &EncodePlan, source: &MediaFile, output: &MediaFile) -> Vec<ValidationCheck> {
    let mut checks = Vec::new();

    // video
    let out_video = output.primary_video();
    checks.push(ValidationCheck::new(
        CheckKind::VideoStream,
        out_video.is_some() == plan.video.is_some(),
        plan.video.is_some(),
        out_video.is_some(),
    ));
    if let (Some(vp), Some(ov)) = (&plan.video, out_video) {
        let expected_codec = match &vp.action {
            VideoAction::Copy => source
                .primary_video()
                .map_or(VideoCodec::Other("?".into()), |v| v.codec.clone()),
            VideoAction::Encode { codec, .. } => match codec {
                TargetVideoCodec::H264 => VideoCodec::H264,
                TargetVideoCodec::Hevc => VideoCodec::Hevc,
            },
        };
        checks.push(ValidationCheck::new(
            CheckKind::VideoCodec,
            ov.codec == expected_codec,
            expected_codec.label(),
            ov.codec.label(),
        ));
        let expected = vp.output_dimensions();
        let actual = crate::analyzer::resolution::Dimensions::new(ov.width, ov.height);
        checks.push(ValidationCheck::new(
            CheckKind::Resolution,
            actual == expected,
            expected,
            actual,
        ));
    }

    // audio
    checks.push(ValidationCheck::new(
        CheckKind::AudioStreams,
        output.audio.len() == plan.audio.len(),
        plan.audio.len(),
        output.audio.len(),
    ));
    let expected_langs: Vec<String> = plan.audio.iter().map(|a| lang(&a.language)).collect();
    let actual_langs: Vec<String> = output.audio.iter().map(|a| lang(&a.language)).collect();
    checks.push(ValidationCheck::new(
        CheckKind::AudioLanguages,
        expected_langs == actual_langs,
        expected_langs.join(","),
        actual_langs.join(","),
    ));

    // Titles that were planned must survive (empty titles are not checked).
    let expected_titles: Vec<String> = plan
        .audio
        .iter()
        .map(|a| a.title.clone().unwrap_or_default())
        .collect();
    let actual_titles: Vec<String> = output
        .audio
        .iter()
        .map(|a| a.title.clone().unwrap_or_default())
        .collect();
    let titles_ok = expected_titles.len() == actual_titles.len()
        && expected_titles
            .iter()
            .zip(&actual_titles)
            .all(|(e, a)| e.is_empty() || e == a);
    if expected_titles.iter().any(|t| !t.is_empty()) {
        checks.push(ValidationCheck::new(
            CheckKind::AudioTitles,
            titles_ok,
            expected_titles.join(" | "),
            actual_titles.join(" | "),
        ));
    }

    // subtitles (embedded mov_text)
    let embedded: Vec<String> = plan
        .subtitles
        .iter()
        .filter(|s| s.action == SubtitleAction::MovText)
        .map(|s| lang(&s.language))
        .collect();
    let actual_subs: Vec<String> = output.subtitles.iter().map(|s| lang(&s.language)).collect();
    checks.push(ValidationCheck::new(
        CheckKind::SubtitleStreams,
        output.subtitles.len() == embedded.len(),
        embedded.len(),
        output.subtitles.len(),
    ));
    if !embedded.is_empty() {
        checks.push(ValidationCheck::new(
            CheckKind::SubtitleLanguages,
            embedded == actual_subs,
            embedded.join(","),
            actual_subs.join(","),
        ));
    }

    // duration
    match (source.duration_s(), output.duration_s()) {
        (Some(src), Some(out)) => checks.push(ValidationCheck::new(
            CheckKind::Duration,
            (src - out).abs() <= duration_tolerance(src),
            format!("{src:.1}s"),
            format!("{out:.1}s"),
        )),
        (Some(src), None) => checks.push(ValidationCheck::new(
            CheckKind::Duration,
            false,
            format!("{src:.1}s"),
            "?",
        )),
        // unknown source duration: cannot compare (warned in the plan)
        (None, _) => {}
    }
    checks
}

/// Inputs for [`validate`].
pub struct ValidateRequest<'a> {
    pub ffmpeg: &'a Path,
    pub ffprobe: &'a Path,
    pub output: &'a Path,
    pub plan: &'a EncodePlan,
    pub source: &'a MediaFile,
    pub sidecars: &'a [PathBuf],
    pub decode_test: bool,
    pub log_file: Option<PathBuf>,
    pub nice: i32,
}

pub async fn validate(req: ValidateRequest<'_>, handle: &ProcessHandle) -> ValidationReport {
    let mut report = ValidationReport::default();
    let meta = tokio::fs::metadata(req.output).await;
    let exists = meta.as_ref().is_ok_and(|m| m.is_file());
    report.checks.push(ValidationCheck::new(
        CheckKind::OutputExists,
        exists,
        req.output.display(),
        if exists { "ok" } else { "missing" },
    ));
    if !exists {
        return report;
    }
    let size = meta.map(|m| m.len()).unwrap_or(0);
    // Plausibility: at least 1 KiB and not absurdly small vs. duration.
    let min_size = req
        .source
        .duration_s()
        .map_or(1024, |d| (d * 1000.0) as u64)
        .max(1024);
    report.checks.push(ValidationCheck::new(
        CheckKind::OutputNotEmpty,
        size >= min_size,
        format!(">= {min_size} B"),
        format!("{size} B"),
    ));

    let facts = FileFacts {
        absolute_path: req.output.to_path_buf(),
        relative_path: req
            .output
            .file_name()
            .map(PathBuf::from)
            .unwrap_or_default(),
        size_bytes: size,
        modified_unix: 0,
    };
    match probe::analyze_file(req.ffprobe, &facts).await {
        Ok(output) => {
            report.checks.push(ValidationCheck::new(
                CheckKind::ProbeReadable,
                true,
                "ok",
                "ok",
            ));
            report.checks.extend(compare(req.plan, req.source, &output));
        }
        Err(err) => {
            report.checks.push(ValidationCheck::new(
                CheckKind::ProbeReadable,
                false,
                "ok",
                err.to_string(),
            ));
            return report;
        }
    }

    for sidecar in req.sidecars {
        // a container header alone (e.g. an empty .mks) is not a usable sidecar:
        // the file must be readable and contain at least one subtitle event
        let exists = tokio::fs::metadata(sidecar)
            .await
            .is_ok_and(|m| m.is_file() && m.len() > 0);
        let packets = if exists {
            crate::media::probe::count_subtitle_packets(req.ffprobe, sidecar, 0)
                .await
                .ok()
        } else {
            None
        };
        let ok = packets.is_some_and(|n| n > 0);
        let actual = match (exists, packets) {
            (false, _) => "missing/empty".to_string(),
            (true, None) => "unreadable".to_string(),
            (true, Some(n)) => format!("{n} packets"),
        };
        report.checks.push(ValidationCheck::new(
            CheckKind::Sidecar,
            ok,
            sidecar.display(),
            actual,
        ));
    }

    if req.decode_test {
        let args: Vec<OsString> = [
            "-hide_banner",
            "-nostdin",
            "-loglevel",
            "error",
            "-progress",
            "pipe:1",
            "-nostats",
            "-i",
        ]
        .iter()
        .map(OsString::from)
        .chain([req.output.as_os_str().to_owned()])
        .chain(
            ["-map", "0:v?", "-map", "0:a?", "-f", "null", "-"]
                .iter()
                .map(OsString::from),
        )
        .collect();
        let result = run_ffmpeg(
            RunOptions {
                ffmpeg: req.ffmpeg.to_path_buf(),
                args,
                log_file: req.log_file.clone(),
                nice: req.nice,
                context: "decode test".into(),
            },
            handle,
            |_| {},
        )
        .await;
        let (passed, actual) = match result {
            Ok(out) => {
                let errors = out
                    .stderr_tail
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .count();
                (errors == 0, format!("{errors} decoder errors"))
            }
            Err(e) => (false, e.to_string()),
        };
        report.checks.push(ValidationCheck::new(
            CheckKind::DecodeTest,
            passed,
            "0 decoder errors",
            actual,
        ));
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::plan::{PlanCapabilities, build_plan};
    use crate::encoding::profile::EncodingProfile;
    use crate::media::fixtures::*;

    fn source() -> MediaFile {
        media(
            "mkv",
            10_000_000_000,
            3600.0,
            vec![video("hevc", 3840, 2160)],
            vec![
                audio("aac", Some("LC"), 2, "ger"),
                audio("aac", Some("LC"), 2, "eng"),
            ],
            vec![subtitle("subrip", "ger")],
        )
    }

    fn good_output() -> MediaFile {
        media(
            "mp4",
            3_000_000_000,
            3600.4,
            vec![video("h264", 1920, 1080)],
            vec![
                audio("aac", Some("LC"), 2, "ger"),
                audio("aac", Some("LC"), 2, "eng"),
            ],
            vec![subtitle("mov_text", "ger")],
        )
    }

    fn plan() -> EncodePlan {
        build_plan(
            &source(),
            &EncodingProfile::universal(),
            &PlanCapabilities::default(),
        )
    }

    #[test]
    fn matching_output_passes() {
        let checks = compare(&plan(), &source(), &good_output());
        assert!(checks.iter().all(|c| c.passed), "{checks:#?}");
    }

    #[test]
    fn detects_missing_audio_and_wrong_language() {
        let mut out = good_output();
        out.audio.pop();
        let checks = compare(&plan(), &source(), &out);
        assert!(
            checks
                .iter()
                .any(|c| c.kind == CheckKind::AudioStreams && !c.passed)
        );
        let mut out = good_output();
        out.audio[1].language = Some("fre".into());
        let checks = compare(&plan(), &source(), &out);
        assert!(
            checks
                .iter()
                .any(|c| c.kind == CheckKind::AudioLanguages && !c.passed)
        );
    }

    #[test]
    fn detects_resolution_duration_and_subtitles() {
        let mut out = good_output();
        out.video[0].width = 3840;
        out.container.duration_s = Some(3000.0);
        out.subtitles.clear();
        let failed: Vec<_> = compare(&plan(), &source(), &out)
            .into_iter()
            .filter(|c| !c.passed)
            .map(|c| c.kind)
            .collect();
        assert!(failed.contains(&CheckKind::Resolution));
        assert!(failed.contains(&CheckKind::Duration));
        assert!(failed.contains(&CheckKind::SubtitleStreams));
    }

    #[test]
    fn tolerance() {
        assert_eq!(duration_tolerance(10.0), 1.5);
        assert!((duration_tolerance(7200.0) - 7.2).abs() < 1e-9);
    }

    #[test]
    fn empty_report_is_not_passed() {
        assert!(!ValidationReport::default().passed());
    }
}
