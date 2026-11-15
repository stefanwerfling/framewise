//! Health of media files: does a file play smoothly on the TV?
//!
//! * Stage 1 ([`risk`]): playback risks derived from the scan data – what
//!   makes the server convert on the fly for the configured device, plus
//!   hints at audio/video drift. Free, computed for every file.
//! * Stage 2 ([`check`]): quick check with ffprobe – header plus a few short
//!   decoded samples, read locally or through the media server's API.
//!   Finds broken data, audio gaps and offsets.

pub mod check;
pub mod risk;

use serde::{Deserialize, Serialize};

use crate::media::model::MediaFile;
use crate::{tr, trf};

/// How bad a finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Severity {
    /// The device probably cannot play it directly (server converts).
    Risk,
    /// Indication of a problem (drift, gaps) – not a proof.
    Suspect,
    /// Broken data or unreadable file.
    Defect,
}

/// One observation about a file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Finding {
    // ---- stage 1: playback risks
    VideoCodec(String),
    /// Leading pictures reference the previous group (open GOP).
    OpenGop,
    TenBitH264,
    Interlaced,
    DolbyVisionWithoutFallback(Option<u8>),
    ResolutionTooHigh(u32),
    BitrateTooHigh(u64),
    AudioCodec(String),
    Subtitles(String),
    Container(String),
    VariableFrameRate,
    NoStreamInfo,
    // ---- stage 2: quick check
    /// MP4 index (`moov`, bytes) after the media data: the player has to
    /// fetch the end of the file before it can start.
    IndexAtEnd(u64),
    Unreadable(String),
    /// Decoder errors in the sample starting at `at` seconds.
    DecodeErrors {
        at: f64,
        count: u32,
        example: String,
    },
    /// Audio starts this many ms after (positive) or before the video.
    AudioOffset(i64),
    /// Duration of video and audio track differ (seconds).
    LengthMismatch {
        video: f64,
        audio: f64,
    },
    AudioGap {
        at: f64,
        ms: i64,
    },
    VideoGap {
        at: f64,
        ms: i64,
    },
    TimestampsBackwards {
        at: f64,
        audio: bool,
    },
}

impl Finding {
    pub fn severity(&self) -> Severity {
        use Finding::*;
        match self {
            Unreadable(_) | DecodeErrors { .. } => Severity::Defect,
            AudioOffset(_)
            | LengthMismatch { .. }
            | AudioGap { .. }
            | VideoGap { .. }
            | TimestampsBackwards { .. } => Severity::Suspect,
            _ => Severity::Risk,
        }
    }

    /// Localised one-line description.
    pub fn describe(&self) -> String {
        use Finding::*;
        match self {
            VideoCodec(c) => trf!("video codec {} is usually converted by the server", c),
            OpenGop => tr!(
                "open GOP – reported to cause audio drift on Fire TV; re-encode with closed GOPs"
            )
            .into(),
            TenBitH264 => tr!("H.264 with 10 bit is rarely decoded in hardware").into(),
            Interlaced => tr!("interlaced video – the server deinterlaces on the fly").into(),
            DolbyVisionWithoutFallback(p) => trf!(
                "Dolby Vision profile {} without HDR10/SDR base – wrong colours or conversion",
                p.map_or("?".into(), |p| p.to_string())
            ),
            ResolutionTooHigh(h) => trf!(
                "{}p – 1080p players (e.g. the plain Fire TV stick) cannot play it",
                h
            ),
            BitrateTooHigh(b) => trf!(
                "video bitrate {} Mbit/s above 20 Mbit/s – not every player decodes it, stalls over Wi-Fi",
                b / 1_000_000
            ),
            AudioCodec(c) => trf!(
                "audio {} is converted by the server unless a receiver takes it",
                c
            ),
            Subtitles(c) => trf!(
                "{} subtitles shown by default are burned in (conversion)",
                c
            ),
            Container(c) => trf!("container {} is remuxed by the server", c),
            VariableFrameRate => tr!("variable frame rate – may drift on TV players").into(),
            NoStreamInfo => {
                tr!("no stream information – the server could not analyse the file").into()
            }
            IndexAtEnd(bytes) => trf!(
                "MP4 index ({} MB) at the end of the file – slow start, players may give up; fix: remux with -movflags +faststart",
                format!("{:.1}", *bytes as f64 / 1_000_000.0)
            ),
            Unreadable(e) => trf!("file cannot be read: {}", e),
            DecodeErrors { at, count, example } => {
                trf!("{} decoder errors at {}: {}", count, timecode(*at), example)
            }
            AudioOffset(ms) => trf!("audio starts {} ms off the video", ms),
            LengthMismatch { video, audio } => trf!(
                "video ({}) and audio ({}) differ in length",
                timecode(*video),
                timecode(*audio)
            ),
            AudioGap { at, ms } => trf!("gap in the audio at {} ({} ms)", timecode(*at), ms),
            VideoGap { at, ms } => trf!("gap in the video at {} ({} ms)", timecode(*at), ms),
            TimestampsBackwards { at, audio } => trf!(
                "{} timestamps jump backwards at {}",
                if *audio { tr!("audio") } else { tr!("video") },
                timecode(*at)
            ),
        }
    }
}

/// `5025.4` → `1:23:45`.
pub fn timecode(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// Result of the quick check (stage 2) for one file version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckReport {
    /// [`fingerprint`] of the file that was checked.
    pub fingerprint: String,
    pub checked_unix: i64,
    pub findings: Vec<Finding>,
    /// Samples that could be decoded.
    pub samples: u32,
    /// The check itself failed (network, timeout) – says nothing about the file.
    pub failed: Option<String>,
    /// [`CHECK_VERSION`] that produced the report.
    #[serde(default)]
    pub version: u32,
}

/// Raised when the quick check learns something new; older reports then
/// count as unchecked. 2: MP4 index position, first sample at 0:00.
/// 3: open GOP.
pub const CHECK_VERSION: u32 = 3;

impl CheckReport {
    /// Valid for the file version with `fingerprint`.
    pub fn is_current(&self, fingerprint: &str) -> bool {
        self.fingerprint == fingerprint && self.version >= CHECK_VERSION
    }
}

/// Identifies a file version: a changed file has to be checked again.
pub fn fingerprint(m: &MediaFile) -> String {
    let source = m
        .provider
        .as_ref()
        .map_or("", |p| p.media_source_id.as_str());
    format!("{}:{}:{}", m.size_bytes, m.modified_unix, source)
}

/// Overall state shown in the library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum HealthState {
    #[default]
    Ok,
    /// Not checked yet (and no playback risk).
    Unchecked,
    /// The quick check could not run.
    CheckFailed,
    Risk,
    Suspect,
    Defect,
}

impl HealthState {
    pub fn label(self) -> &'static str {
        match self {
            HealthState::Ok => tr!("OK"),
            HealthState::Unchecked => tr!("not checked"),
            HealthState::CheckFailed => tr!("check failed"),
            HealthState::Risk => tr!("playback risk"),
            HealthState::Suspect => tr!("suspect"),
            HealthState::Defect => tr!("defect"),
        }
    }

    pub fn is_problem(self) -> bool {
        matches!(
            self,
            HealthState::Risk | HealthState::Suspect | HealthState::Defect
        )
    }
}

/// Stage 1 risks plus the stored quick check of the current file version.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Health {
    pub risks: Vec<Finding>,
    pub check: Option<CheckReport>,
}

impl Health {
    pub fn state(&self) -> HealthState {
        let worst = self
            .risks
            .iter()
            .chain(self.check.iter().flat_map(|c| &c.findings))
            .map(Finding::severity)
            .max();
        match worst {
            Some(Severity::Defect) => HealthState::Defect,
            Some(Severity::Suspect) => HealthState::Suspect,
            _ if self.check.as_ref().is_some_and(|c| c.failed.is_some()) => {
                HealthState::CheckFailed
            }
            Some(Severity::Risk) => HealthState::Risk,
            None if self.check.is_none() => HealthState::Unchecked,
            None => HealthState::Ok,
        }
    }

    /// All findings, worst first.
    pub fn findings(&self) -> Vec<&Finding> {
        let mut all: Vec<&Finding> = self
            .check
            .iter()
            .flat_map(|c| &c.findings)
            .chain(&self.risks)
            .collect();
        all.sort_by_key(|f| std::cmp::Reverse(f.severity()));
        all
    }

    pub fn checked(&self) -> bool {
        self.check.as_ref().is_some_and(|c| c.failed.is_none())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(findings: Vec<Finding>, failed: Option<&str>) -> CheckReport {
        CheckReport {
            fingerprint: String::new(),
            checked_unix: 0,
            findings,
            samples: 6,
            failed: failed.map(str::to_string),
            version: CHECK_VERSION,
        }
    }

    #[test]
    fn state_takes_the_worst_finding() {
        let mut h = Health::default();
        assert_eq!(h.state(), HealthState::Unchecked);
        h.risks.push(Finding::Interlaced);
        assert_eq!(h.state(), HealthState::Risk);
        h.check = Some(report(vec![], None));
        assert_eq!(h.state(), HealthState::Risk);
        h.risks.clear();
        assert_eq!(h.state(), HealthState::Ok);
        h.check = Some(report(vec![], Some("timeout")));
        assert_eq!(h.state(), HealthState::CheckFailed);
        h.check = Some(report(vec![Finding::AudioOffset(400)], None));
        assert_eq!(h.state(), HealthState::Suspect);
        h.check = Some(report(
            vec![
                Finding::AudioOffset(400),
                Finding::DecodeErrors {
                    at: 10.0,
                    count: 3,
                    example: "x".into(),
                },
            ],
            None,
        ));
        h.risks.push(Finding::Interlaced);
        assert_eq!(h.state(), HealthState::Defect);
        assert_eq!(h.findings()[0].severity(), Severity::Defect);
        assert_eq!(timecode(5025.4), "1:23:45");
    }
}
