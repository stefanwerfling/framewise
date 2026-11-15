//! Stage 2: quick check with ffprobe.
//!
//! 1. Header: start time and length of the video and the main audio track.
//! 2. A few short samples spread over the file are decoded (`-show_frames`
//!    with `-read_intervals`): decoder errors, gaps and backwards jumps in the
//!    timestamps.
//!
//! The input is a local file or an HTTP URL (media server). Over HTTP ffprobe
//! seeks with range requests, so only the samples are transferred.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use super::{CheckReport, Finding};
use crate::core::tools::run_capture;
use crate::tr;

const HEADER_TIMEOUT: Duration = Duration::from_secs(90);
const SAMPLE_TIMEOUT: Duration = Duration::from_secs(180);
/// Audio starting this much before/after the video is reported.
const OFFSET_MS: i64 = 100;
/// Track lengths differing by more than this (seconds) are reported.
const LENGTH_DIFF_S: f64 = 3.0;
const AUDIO_GAP_S: f64 = 0.1;
const VIDEO_GAP_S: f64 = 0.5;

/// Where ffprobe reads the file from.
#[derive(Debug, Clone)]
pub enum Input {
    File(PathBuf),
    Http {
        url: String,
        /// Value of the `Authorization` header.
        authorization: String,
    },
}

impl Input {
    fn args(&self) -> Vec<OsString> {
        match self {
            Input::File(path) => vec!["-i".into(), path.into()],
            Input::Http { url, authorization } => vec![
                "-headers".into(),
                format!("Authorization: {authorization}\r\n").into(),
                // Survive short network hiccups of the server.
                "-reconnect".into(),
                "1".into(),
                "-reconnect_on_network_error".into(),
                "1".into(),
                "-i".into(),
                url.into(),
            ],
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CheckOptions {
    pub samples: u32,
    pub sample_seconds: u32,
}

/// Run the quick check. Returns `None` if cancelled.
pub async fn run(
    ffprobe: &std::path::Path,
    input: &Input,
    fingerprint: String,
    options: CheckOptions,
    cancel: &Arc<AtomicBool>,
) -> Option<CheckReport> {
    let mut report = CheckReport {
        fingerprint,
        checked_unix: crate::queue::job::now_unix(),
        findings: Vec::new(),
        samples: 0,
        failed: None,
        version: super::CHECK_VERSION,
    };

    let header = match probe_header(ffprobe, input).await {
        Ok(h) => h,
        Err(HeaderError::Unreadable(e)) => {
            report.findings.push(Finding::Unreadable(e));
            return Some(report);
        }
        Err(HeaderError::Failed(e)) => {
            report.failed = Some(e);
            return Some(report);
        }
    };
    report.findings.extend(header.findings());
    match index_at_end(input).await {
        Ok(Some(bytes)) => report.findings.push(Finding::IndexAtEnd(bytes)),
        Ok(None) => {}
        // Not essential: the samples below still run.
        Err(e) => tracing::debug!("MP4 layout not read: {e}"),
    }

    for at in sample_points(header.duration, options) {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let streams = [header.video, header.audio];
        let mut result = sample(ffprobe, input, at, options.sample_seconds, &streams).await;
        if result.is_err() {
            // One retry: a network hiccup is not a defect.
            result = sample(ffprobe, input, at, options.sample_seconds, &streams).await;
        }
        match result {
            Ok(out) => {
                report.samples += 1;
                report
                    .findings
                    .extend(analyze_sample(&out, at, header.video, header.audio));
            }
            Err(e) => {
                report.failed = Some(e);
                break;
            }
        }
    }
    Some(report)
}

// ------------------------------------------------------------------ header

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Header {
    pub duration: Option<f64>,
    /// Stream indices of the video and main audio track.
    pub video: Option<u32>,
    pub audio: Option<u32>,
    video_start: Option<f64>,
    audio_start: Option<f64>,
    video_length: Option<f64>,
    audio_length: Option<f64>,
}

impl Header {
    pub fn findings(&self) -> Vec<Finding> {
        let mut out = Vec::new();
        if let (Some(v), Some(a)) = (self.video_start, self.audio_start) {
            let ms = ((a - v) * 1000.0).round() as i64;
            if ms.abs() > OFFSET_MS {
                out.push(Finding::AudioOffset(ms));
            }
        }
        if let (Some(v), Some(a)) = (self.video_length, self.audio_length)
            && (v - a).abs() > LENGTH_DIFF_S
        {
            out.push(Finding::LengthMismatch { video: v, audio: a });
        }
        out
    }
}

enum HeaderError {
    /// ffprobe ran but could not make sense of the file.
    Unreadable(String),
    /// The check could not run (network, timeout …).
    Failed(String),
}

async fn probe_header(ffprobe: &std::path::Path, input: &Input) -> Result<Header, HeaderError> {
    let mut args: Vec<OsString> = [
        "-v",
        "error",
        "-show_entries",
        "stream=index,codec_type,start_time,duration:stream_tags=DURATION:stream_disposition=default,attached_pic:format=duration",
        "-of",
        "json",
    ]
    .iter()
    .map(Into::into)
    .collect();
    args.extend(input.args());
    match run_capture("ffprobe", ffprobe, args, HEADER_TIMEOUT, "health check").await {
        Ok(out) => parse_header(&out.stdout).ok_or_else(|| {
            HeaderError::Unreadable(
                first_line(&out.stderr)
                    .unwrap_or(tr!("no video or audio track found"))
                    .into(),
            )
        }),
        Err(e) => {
            let text = e.to_string();
            if is_network_error(&text) {
                Err(HeaderError::Failed(text))
            } else {
                Err(HeaderError::Unreadable(text))
            }
        }
    }
}

fn is_network_error(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    [
        "timed out",
        "connection",
        "http error",
        "server returned",
        "network",
        "resolve",
        "tls",
        "i/o error",
        "end of file",
    ]
    .iter()
    .any(|p| t.contains(p))
}

fn first_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|l| !l.is_empty())
}

#[derive(Deserialize)]
struct ProbeJson {
    #[serde(default)]
    streams: Vec<Value>,
    #[serde(default)]
    format: Value,
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
    .filter(|x: &f64| x.is_finite())
}

/// `01:40:35.840000000` → seconds.
fn parse_clock(text: &str) -> Option<f64> {
    let mut parts = text.trim().split(':');
    let h: f64 = parts.next()?.parse().ok()?;
    let m: f64 = parts.next()?.parse().ok()?;
    let s: f64 = parts.next()?.parse().ok()?;
    Some(h * 3600.0 + m * 60.0 + s)
}

pub fn parse_header(json: &[u8]) -> Option<Header> {
    let p: ProbeJson = serde_json::from_slice(json).ok()?;
    let flag = |s: &Value, k: &str| s["disposition"][k].as_i64() == Some(1);
    let length = |s: &Value| {
        num(&s["duration"]).or_else(|| s["tags"]["DURATION"].as_str().and_then(parse_clock))
    };
    let video = p
        .streams
        .iter()
        .find(|s| s["codec_type"] == "video" && !flag(s, "attached_pic"));
    let audios: Vec<&Value> = p
        .streams
        .iter()
        .filter(|s| s["codec_type"] == "audio")
        .collect();
    let audio = audios
        .iter()
        .find(|s| flag(s, "default"))
        .or(audios.first())
        .copied();
    if video.is_none() && audio.is_none() {
        return None;
    }
    let index = |s: &Value| s["index"].as_u64().map(|i| i as u32);
    Some(Header {
        duration: num(&p.format["duration"])
            .or_else(|| video.and_then(length))
            .or_else(|| audio.and_then(length)),
        video: video.and_then(index),
        audio: audio.and_then(index),
        video_start: video.and_then(|s| num(&s["start_time"])),
        audio_start: audio.and_then(|s| num(&s["start_time"])),
        video_length: video.and_then(length),
        audio_length: audio.and_then(length),
    })
}

// -------------------------------------------------------------- MP4 layout

/// Top-level boxes read at most (MP4 files have a handful).
const MAX_BOXES: usize = 64;

/// Size of the MP4 index (`moov`) if it comes after the media data (`mdat`).
/// `None` for other containers and for files that start with the index.
pub async fn index_at_end(input: &Input) -> Result<Option<u64>, String> {
    let mut reader = RangeReader::new(input)?;
    let mut boxes = Vec::new();
    let mut pos = 0u64;
    while boxes.len() < MAX_BOXES {
        let head = reader.read(pos, 16).await?;
        let Some((kind, size)) = parse_box_header(&head) else {
            break;
        };
        if boxes.is_empty() && kind != "ftyp" {
            return Ok(None);
        }
        let last = size == 0 || kind == "moov";
        boxes.push((kind, size));
        if last {
            break;
        }
        pos += size;
    }
    Ok(layout_index_at_end(&boxes))
}

/// `(type, size)` of a box header; size 0 = up to the end of the file.
pub fn parse_box_header(head: &[u8]) -> Option<(String, u64)> {
    if head.len() < 8 {
        return None;
    }
    let size32 = u32::from_be_bytes(head[0..4].try_into().ok()?);
    let kind: String = head[4..8].iter().map(|&b| b as char).collect();
    if !kind.chars().all(|c| c.is_ascii_graphic() || c == ' ') {
        return None;
    }
    let size = match size32 {
        1 => u64::from_be_bytes(head.get(8..16)?.try_into().ok()?),
        n => u64::from(n),
    };
    // Smaller than its own header: not an MP4 box.
    if size != 0 && size < 8 {
        return None;
    }
    Some((kind, size))
}

pub fn layout_index_at_end(boxes: &[(String, u64)]) -> Option<u64> {
    if boxes.first().map(|(k, _)| k.as_str()) != Some("ftyp") {
        return None;
    }
    let mdat = boxes.iter().position(|(k, _)| k == "mdat")?;
    let moov = boxes.iter().position(|(k, _)| k == "moov")?;
    (moov > mdat).then(|| boxes[moov].1)
}

/// Reads byte ranges of a local file or over HTTP (range requests).
enum RangeReader {
    File(std::path::PathBuf),
    Http {
        client: reqwest::Client,
        url: String,
        authorization: String,
    },
}

impl RangeReader {
    fn new(input: &Input) -> Result<Self, String> {
        Ok(match input {
            Input::File(path) => RangeReader::File(path.clone()),
            Input::Http { url, authorization } => RangeReader::Http {
                client: reqwest::Client::builder()
                    .timeout(Duration::from_secs(30))
                    .build()
                    .map_err(|e| e.to_string())?,
                url: url.clone(),
                authorization: authorization.clone(),
            },
        })
    }

    async fn read(&mut self, pos: u64, len: u64) -> Result<Vec<u8>, String> {
        match self {
            RangeReader::File(path) => {
                use tokio::io::{AsyncReadExt, AsyncSeekExt};
                let mut f = tokio::fs::File::open(&path)
                    .await
                    .map_err(|e| e.to_string())?;
                f.seek(std::io::SeekFrom::Start(pos))
                    .await
                    .map_err(|e| e.to_string())?;
                let mut buf = Vec::with_capacity(len as usize);
                f.take(len)
                    .read_to_end(&mut buf)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(buf)
            }
            RangeReader::Http {
                client,
                url,
                authorization,
            } => {
                let response = client
                    .get(url.as_str())
                    .header(reqwest::header::AUTHORIZATION, authorization.as_str())
                    .header(
                        reqwest::header::RANGE,
                        format!("bytes={pos}-{}", pos + len - 1),
                    )
                    .send()
                    .await
                    .map_err(|e| e.to_string())?;
                match response.status().as_u16() {
                    206 => Ok(response.bytes().await.map_err(|e| e.to_string())?.to_vec()),
                    // Past the end of the file.
                    416 => Ok(Vec::new()),
                    // 200 would send the whole file – drop it unread.
                    status => Err(format!("range request answered with {status}")),
                }
            }
        }
    }
}

// ----------------------------------------------------------------- samples

/// Start times of the samples, spread evenly and away from the very end.
pub fn sample_points(duration: Option<f64>, options: CheckOptions) -> Vec<f64> {
    let n = options.samples.max(1);
    let len = f64::from(options.sample_seconds.max(1));
    match duration.filter(|d| *d > len * 2.0) {
        None => vec![0.0],
        // The start first: problems there stop playback right away.
        Some(d) => (0..n)
            .map(|i| {
                let at = if i == 0 {
                    0.0
                } else {
                    d * (f64::from(i) + 0.5) / f64::from(n)
                };
                at.clamp(0.0, (d - len - 2.0).max(0.0)).floor()
            })
            .collect(),
    }
}

/// Raw result of one sample.
#[derive(Debug, Clone, Default)]
pub struct SampleOutput {
    pub frames: Vec<FrameInfo>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameInfo {
    pub stream: u32,
    pub audio: bool,
    pub time: f64,
    pub duration: Option<f64>,
}

/// Decode one sample of each given stream. The streams are read one at a
/// time: ffprobe cannot select several, and other streams (cover art with
/// timestamp 0 in particular) break the read interval.
async fn sample(
    ffprobe: &std::path::Path,
    input: &Input,
    at: f64,
    seconds: u32,
    streams: &[Option<u32>],
) -> Result<SampleOutput, String> {
    let mut all = SampleOutput::default();
    for &index in streams.iter().flatten() {
        let out = sample_stream(ffprobe, input, at, seconds, index).await?;
        all.frames.extend(out.frames);
        all.errors.extend(out.errors);
    }
    Ok(all)
}

async fn sample_stream(
    ffprobe: &std::path::Path,
    input: &Input,
    at: f64,
    seconds: u32,
    index: u32,
) -> Result<SampleOutput, String> {
    let mut args: Vec<OsString> = vec![
        "-v".into(),
        "error".into(),
        "-select_streams".into(),
        index.to_string().into(),
        "-read_intervals".into(),
        format!("{at:.3}%+{seconds}").into(),
        "-show_entries".into(),
        "frame=media_type,stream_index,best_effort_timestamp_time,pts_time,duration_time".into(),
        "-of".into(),
        "json".into(),
    ];
    args.extend(input.args());
    let out = run_capture("ffprobe", ffprobe, args, SAMPLE_TIMEOUT, "health check")
        .await
        .map_err(|e| e.to_string())?;
    if is_network_error(&out.stderr) && out.stdout.len() < 64 {
        return Err(first_line(&out.stderr).unwrap_or_default().to_string());
    }
    Ok(parse_sample(&out.stdout, &out.stderr))
}

pub fn parse_sample(json: &[u8], stderr: &str) -> SampleOutput {
    let frames = serde_json::from_slice::<Value>(json)
        .ok()
        .and_then(|v| v["frames"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|f| {
            let kind = f["media_type"].as_str()?;
            if kind != "video" && kind != "audio" {
                return None;
            }
            Some(FrameInfo {
                stream: f["stream_index"].as_u64()? as u32,
                audio: kind == "audio",
                time: num(&f["best_effort_timestamp_time"]).or_else(|| num(&f["pts_time"]))?,
                duration: num(&f["duration_time"]),
            })
        })
        .collect();
    let errors = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    SampleOutput { frames, errors }
}

/// Decoder messages that appear right after seeking into a stream (leading
/// pictures referencing the previous group) – not a defect.
const SEEK_NOISE: &[&str] = &[
    "could not find ref with poc",
    "co located pocs unavailable",
    "missing reference picture",
    "number of reference frames",
    "skipping invalid undecodable nalu",
    "header missing",
    "last message repeated",
];

/// Messages that clearly mean broken data.
const CORRUPTION: &[&str] = &[
    "corrupt",
    "concealing",
    "error while decoding",
    "invalid nal",
    "invalid data",
    "decode_slice_header error",
    "illegal",
    "damaged",
    "overread",
    "out of range",
    "error splitting",
    "non-existing pps",
    "non-existing sps",
    "invalid frame",
];

pub fn analyze_sample(
    out: &SampleOutput,
    at: f64,
    video: Option<u32>,
    audio: Option<u32>,
) -> Vec<Finding> {
    let mut findings = Vec::new();

    let errors: Vec<&String> = out
        .errors
        .iter()
        .filter(|e| {
            let l = e.to_ascii_lowercase();
            !SEEK_NOISE.iter().any(|n| l.contains(n))
        })
        .collect();
    let strong = errors.iter().any(|e| {
        let l = e.to_ascii_lowercase();
        CORRUPTION.iter().any(|c| l.contains(c))
    });
    let video_frames: Vec<&FrameInfo> = out
        .frames
        .iter()
        .filter(|f| !f.audio && Some(f.stream) == video)
        .collect();
    if strong || errors.len() >= 3 {
        findings.push(Finding::DecodeErrors {
            at,
            count: errors.len() as u32,
            example: errors.first().map(|e| strip_prefix(e)).unwrap_or_default(),
        });
    } else if video.is_some() && video_frames.is_empty() {
        findings.push(Finding::DecodeErrors {
            at,
            count: errors.len() as u32,
            example: tr!("no picture could be decoded").into(),
        });
    }

    // Video: output is in presentation order.
    let times: Vec<f64> = video_frames.iter().map(|f| f.time).collect();
    if let Some(i) = (1..times.len()).find(|&i| times[i] < times[i - 1] - 0.001) {
        findings.push(Finding::TimestampsBackwards {
            at: times[i - 1],
            audio: false,
        });
    } else if let Some((t, gap)) = times
        .windows(2)
        .map(|w| (w[0], w[1] - w[0]))
        .find(|(_, gap)| *gap > VIDEO_GAP_S)
    {
        findings.push(Finding::VideoGap {
            at: t,
            ms: (gap * 1000.0).round() as i64,
        });
    }

    // Audio: each frame should start where the previous one ended.
    let audio_frames: Vec<&FrameInfo> = out
        .frames
        .iter()
        .filter(|f| f.audio && Some(f.stream) == audio)
        .collect();
    for w in audio_frames.windows(2) {
        let (a, b) = (w[0], w[1]);
        if b.time < a.time - 0.001 {
            findings.push(Finding::TimestampsBackwards {
                at: a.time,
                audio: true,
            });
            break;
        }
        let end = a.time + a.duration.unwrap_or(0.0);
        if a.duration.is_some() && b.time - end > AUDIO_GAP_S {
            findings.push(Finding::AudioGap {
                at: end,
                ms: ((b.time - end) * 1000.0).round() as i64,
            });
            break;
        }
    }
    findings
}

/// `[h264 @ 0x55d…] error while decoding MB 3 4` → `h264: error while …`.
fn strip_prefix(line: &str) -> String {
    if let Some(rest) = line.strip_prefix('[')
        && let Some((tag, msg)) = rest.split_once(']')
    {
        let codec = tag.split(" @").next().unwrap_or(tag);
        return format!("{codec}: {}", msg.trim());
    }
    line.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = r#"{
      "streams": [
        {"index": 0, "codec_type": "video", "start_time": "0.042000",
         "disposition": {"default": 1, "attached_pic": 0},
         "tags": {"DURATION": "01:40:00.000000000"}},
        {"index": 1, "codec_type": "audio", "start_time": "0.000000",
         "disposition": {"default": 0, "attached_pic": 0},
         "tags": {"DURATION": "01:40:00.020000000"}},
        {"index": 2, "codec_type": "audio", "start_time": "0.700000",
         "disposition": {"default": 1, "attached_pic": 0},
         "tags": {"DURATION": "01:39:50.000000000"}},
        {"index": 3, "codec_type": "video", "disposition": {"default": 0, "attached_pic": 1}}
      ],
      "format": {"duration": "6000.050000"}
    }"#;

    #[test]
    fn header_offset_and_length() {
        let h = parse_header(HEADER.as_bytes()).unwrap();
        assert_eq!(
            (h.video, h.audio),
            (Some(0), Some(2)),
            "default audio track"
        );
        assert_eq!(h.duration, Some(6000.05));
        assert_eq!(
            h.findings(),
            [
                Finding::AudioOffset(658),
                Finding::LengthMismatch {
                    video: 6000.0,
                    audio: 5990.0
                }
            ]
        );
        assert!(parse_header(br#"{"streams": [], "format": {}}"#).is_none());
    }

    #[test]
    fn samples_are_spread_and_stay_inside() {
        let o = CheckOptions {
            samples: 6,
            sample_seconds: 3,
        };
        assert_eq!(
            sample_points(Some(6000.0), o),
            [0.0, 1500.0, 2500.0, 3500.0, 4500.0, 5500.0]
        );
        let short = sample_points(Some(10.0), o);
        assert!(short.iter().all(|&t| t + 3.0 <= 10.0), "{short:?}");
        assert_eq!(sample_points(None, o), [0.0]);
    }

    fn frames(video: &[f64], audio: &[(f64, f64)]) -> Vec<FrameInfo> {
        let mut out: Vec<FrameInfo> = video
            .iter()
            .map(|&t| FrameInfo {
                stream: 0,
                audio: false,
                time: t,
                duration: Some(0.04),
            })
            .collect();
        out.extend(audio.iter().map(|&(t, d)| FrameInfo {
            stream: 1,
            audio: true,
            time: t,
            duration: Some(d),
        }));
        out
    }

    #[test]
    fn clean_sample_has_no_findings() {
        let video: Vec<f64> = (0..75).map(|i| 100.0 + f64::from(i) * 0.04).collect();
        let audio: Vec<(f64, f64)> = (0..94)
            .map(|i| (100.0 + f64::from(i) * 0.032, 0.032))
            .collect();
        let out = SampleOutput {
            frames: frames(&video, &audio),
            errors: vec!["[hevc @ 0x1] Could not find ref with POC 12".into()],
        };
        assert!(analyze_sample(&out, 100.0, Some(0), Some(1)).is_empty());
    }

    #[test]
    fn detects_errors_gaps_and_jumps() {
        let out = SampleOutput {
            frames: frames(
                &[10.0, 10.04, 10.08, 11.2, 11.24],
                &[(10.0, 0.032), (10.032, 0.032), (10.5, 0.032)],
            ),
            errors: vec!["[h264 @ 0x55d] error while decoding MB 3 4, bytestream -5".into()],
        };
        let f = analyze_sample(&out, 10.0, Some(0), Some(1));
        assert_eq!(
            f,
            [
                Finding::DecodeErrors {
                    at: 10.0,
                    count: 1,
                    example: "h264: error while decoding MB 3 4, bytestream -5".into()
                },
                Finding::VideoGap {
                    at: 10.08,
                    ms: 1120
                },
                Finding::AudioGap {
                    at: 10.064,
                    ms: 436
                },
            ]
        );

        let back = SampleOutput {
            frames: frames(&[5.0, 5.04, 4.0], &[(5.0, 0.03), (4.9, 0.03)]),
            errors: vec![],
        };
        let f = analyze_sample(&back, 5.0, Some(0), Some(1));
        assert!(f.contains(&Finding::TimestampsBackwards {
            at: 5.04,
            audio: false
        }));
        assert!(f.contains(&Finding::TimestampsBackwards {
            at: 5.0,
            audio: true
        }));

        let nothing = SampleOutput::default();
        assert!(matches!(
            analyze_sample(&nothing, 1.0, Some(0), Some(1))[..],
            [Finding::DecodeErrors { count: 0, .. }]
        ));
    }

    fn boxes(list: &[(&str, u64)]) -> Vec<(String, u64)> {
        list.iter().map(|(k, s)| (k.to_string(), *s)).collect()
    }

    #[test]
    fn mp4_index_position() {
        let mut head = 24u32.to_be_bytes().to_vec();
        head.extend(b"ftypisom");
        assert_eq!(parse_box_header(&head), Some(("ftyp".into(), 24)));
        let mut large = 1u32.to_be_bytes().to_vec();
        large.extend(b"mdat");
        large.extend(4_491_870_000u64.to_be_bytes());
        assert_eq!(
            parse_box_header(&large),
            Some(("mdat".into(), 4_491_870_000))
        );
        // Matroska starts with an EBML header, not a box.
        assert_eq!(
            parse_box_header(&[0x1a, 0x45, 0xdf, 0xa3, 0x9f, 0x42, 0x86, 0x81]),
            None
        );

        let end = boxes(&[
            ("ftyp", 32),
            ("free", 8),
            ("mdat", 4_000_000),
            ("moov", 8_090_000),
        ]);
        assert_eq!(layout_index_at_end(&end), Some(8_090_000));
        let front = boxes(&[("ftyp", 32), ("moov", 8_090_000), ("mdat", 4_000_000)]);
        assert_eq!(layout_index_at_end(&front), None);
        assert_eq!(
            layout_index_at_end(&boxes(&[("ftyp", 32), ("mdat", 0)])),
            None
        );
    }

    #[test]
    fn parses_ffprobe_frames() {
        let json = br#"{"frames": [
          {"media_type": "video", "stream_index": 0, "best_effort_timestamp_time": "1.000000", "duration_time": "0.040000"},
          {"media_type": "audio", "stream_index": 1, "pts_time": "1.010000", "duration_time": "0.032000"},
          {"media_type": "subtitle", "stream_index": 2, "pts_time": "1.0"},
          {"media_type": "video", "stream_index": 0}
        ]}"#;
        let s = parse_sample(json, "\n[aac @ 0x1] Input buffer exhausted\n");
        assert_eq!(s.frames.len(), 2);
        assert_eq!(s.frames[1].time, 1.01);
        assert_eq!(s.errors, ["[aac @ 0x1] Input buffer exhausted"]);
    }
}
