//! Black bar detection with ffmpeg's `cropdetect`.
//!
//! A few keyframes are decoded at evenly spaced points of the film (intro and
//! credits left out). Each point reports the bounding box of everything that
//! is not black; the result is the **union** of all boxes. A bar is therefore
//! only removed if it is black in every sample: dark scenes cannot shrink the
//! picture, and films with changing aspect ratios (e.g. IMAX scenes) keep the
//! larger frame as soon as one sample hits an open scene.
//!
//! Further safety rules:
//! * at least half of the points must deliver a usable measurement,
//! * an edge is only cropped if the bars on that axis add up to at least 1 %
//!   of the frame (8 px minimum) – single noisy lines are left alone,
//! * the remaining picture must keep at least 40 % of width and height,
//! * offsets are rounded outward to even values, so no picture is lost.

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use super::process::ProcessHandle;
use crate::analyzer::resolution::{CropRect, Dimensions};
use crate::core::error::{Error, Result};
use crate::core::tools::run_capture;

/// Measuring points over the film.
pub const POINTS: u32 = 10;
/// Decoded keyframes per point (`cropdetect` skips the first two).
const FRAMES_PER_POINT: u32 = 6;

/// Non-black area reported by `cropdetect` (inclusive pixel coordinates).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    pub x1: u32,
    pub x2: u32,
    pub y1: u32,
    pub y2: u32,
}

impl Bounds {
    fn union(self, o: Bounds) -> Bounds {
        Bounds {
            x1: self.x1.min(o.x1),
            x2: self.x2.max(o.x2),
            y1: self.y1.min(o.y1),
            y2: self.y2.max(o.y2),
        }
    }
}

/// Last usable `cropdetect` line of one run. With `reset=0` every line holds
/// the box accumulated over all frames so far.
pub fn parse_bounds(stderr: &str) -> Option<Bounds> {
    stderr
        .lines()
        .rev()
        .filter(|l| l.contains("cropdetect"))
        .find_map(|line| {
            let field = |name: &str| -> Option<i64> {
                line.split_whitespace()
                    .find_map(|t| t.strip_prefix(name)?.strip_prefix(':'))
                    .and_then(|v| v.parse().ok())
            };
            let (x1, x2, y1, y2) = (field("x1")?, field("x2")?, field("y1")?, field("y2")?);
            // An all-black frame reports x2 < x1.
            (x1 >= 0 && y1 >= 0 && x2 > x1 && y2 > y1).then_some(Bounds {
                x1: x1 as u32,
                x2: x2 as u32,
                y1: y1 as u32,
                y2: y2 as u32,
            })
        })
}

/// One axis: start and length of the kept range, or the full range if the
/// bars are too small to bother.
fn axis(first: u32, last: u32, size: u32) -> (u32, u32) {
    let last = last.min(size.saturating_sub(1));
    let start = first & !1;
    let end = ((last + 2) & !1).min(size & !1).max(start + 2);
    let removed = size - (end - start);
    let threshold = (size / 100).max(8);
    if removed < threshold {
        (0, size)
    } else {
        (start, end - start)
    }
}

/// Combines the measurements of all points into a crop, or `None` if the
/// full frame should be kept.
pub fn decide(source: Dimensions, measurements: &[Option<Bounds>]) -> Option<CropRect> {
    let valid: Vec<Bounds> = measurements.iter().flatten().copied().collect();
    let needed = measurements.len().div_ceil(2).max(1);
    if valid.len() < needed {
        return None;
    }
    let b = valid.into_iter().reduce(Bounds::union)?;
    let (x, width) = axis(b.x1, b.x2, source.width);
    let (y, height) = axis(b.y1, b.y2, source.height);
    let crop = CropRect {
        width,
        height,
        x,
        y,
    };
    let big_enough = u64::from(width) * 10 >= u64::from(source.width) * 4
        && u64::from(height) * 10 >= u64::from(source.height) * 4;
    (big_enough && crop.dimensions() != source && crop.fits(source)).then_some(crop)
}

/// Measuring points: evenly spread between 10 % and 90 % of the film.
pub fn points(duration_s: f64) -> Vec<f64> {
    if !(duration_s.is_finite() && duration_s > 0.0) {
        return Vec::new();
    }
    let (from, span) = (duration_s * 0.1, duration_s * 0.8);
    (0..POINTS)
        .map(|i| from + span * (f64::from(i) + 0.5) / f64::from(POINTS))
        .collect()
}

fn args(file: &Path, type_index: u32, at: f64) -> Vec<OsString> {
    vec![
        "-hide_banner".into(),
        "-nostdin".into(),
        "-loglevel".into(),
        "info".into(),
        // keyframes only: fast even for UHD HEVC
        "-skip_frame".into(),
        "nokey".into(),
        "-ss".into(),
        format!("{at:.3}").into(),
        "-i".into(),
        file.as_os_str().to_owned(),
        "-map".into(),
        format!("0:v:{type_index}").into(),
        "-vf".into(),
        "cropdetect=round=2:reset=0".into(),
        "-frames:v".into(),
        FRAMES_PER_POINT.to_string().into(),
        "-an".into(),
        "-sn".into(),
        "-dn".into(),
        "-f".into(),
        "null".into(),
        "-".into(),
    ]
}

/// Detects black bars of video stream `v:<type_index>`. `source` is the
/// displayed size (ffmpeg auto-rotates before the filter, as in the encode).
/// Failed points count as missing measurements; cancellable through `handle`.
pub async fn detect(
    ffmpeg: &Path,
    file: &Path,
    type_index: u32,
    source: Dimensions,
    duration_s: f64,
    handle: &ProcessHandle,
) -> Result<Option<CropRect>> {
    let mut measurements = Vec::new();
    for at in points(duration_s) {
        let run = run_capture(
            "ffmpeg",
            ffmpeg,
            args(file, type_index, at),
            Duration::from_secs(120),
            "black bar detection",
        );
        // Dropping the future kills ffmpeg (kill_on_drop).
        let out = tokio::select! {
            out = run => out,
            () = handle.cancelled() => return Err(Error::Cancelled),
        };
        match out {
            Ok(out) => measurements.push(parse_bounds(&out.stderr)),
            Err(e) => {
                tracing::debug!("cropdetect at {at:.0} s failed: {e}");
                measurements.push(None);
            }
        }
    }
    Ok(decide(source, &measurements))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FHD: Dimensions = Dimensions {
        width: 1920,
        height: 1080,
    };

    fn b(x1: u32, x2: u32, y1: u32, y2: u32) -> Option<Bounds> {
        Some(Bounds { x1, x2, y1, y2 })
    }

    #[test]
    fn parses_old_and_new_ffmpeg_output() {
        let v4 = "[Parsed_cropdetect_0 @ 0x55] x1:0 x2:1919 y1:138 y2:941 w:1920 h:800 x:0 y:140 pts:1001 t:0.041708 crop=1920:800:0:140\n";
        let v8 = "[Parsed_cropdetect_0 @ 0x7f] x1:2 x2:1917 y1:140 y2:939 w:1914 h:800 x:2 y:140 pts:12 t:0.500000 limit:0.094118 crop=1914:800:2:140\n";
        assert_eq!(parse_bounds(v4), b(0, 1919, 138, 941));
        assert_eq!(parse_bounds(&format!("{v4}{v8}")), b(2, 1917, 140, 939));
    }

    #[test]
    fn black_frames_are_ignored() {
        let black = "[Parsed_cropdetect_0 @ 0x1] x1:1919 x2:0 y1:1079 y2:0 w:-1918 h:-1078 x:1920 y:1080 pts:0 t:0 crop=-1918:-1078:1920:1080\n";
        assert_eq!(parse_bounds(black), None);
        assert_eq!(parse_bounds("frame=    6 fps=0.0\n"), None);
    }

    #[test]
    fn letterbox_is_cropped_with_even_values() {
        let m = vec![b(0, 1919, 139, 940); 8];
        let c = decide(FHD, &m).unwrap();
        assert_eq!(
            c,
            CropRect {
                width: 1920,
                height: 804,
                x: 0,
                y: 138
            }
        );
        assert!(
            [c.x, c.y, c.width, c.height]
                .iter()
                .all(|v| v.is_multiple_of(2))
        );
    }

    #[test]
    fn union_keeps_the_largest_picture() {
        // Dark scene (small box) and an IMAX scene (full height) in between.
        let m = vec![
            b(0, 1919, 140, 939),
            b(600, 1300, 400, 700),
            b(0, 1919, 140, 939),
            b(0, 1919, 0, 1079),
        ];
        assert_eq!(decide(FHD, &m), None);
    }

    #[test]
    fn small_or_implausible_bars_are_kept() {
        // 4 px noise at the top only.
        assert_eq!(decide(FHD, &[b(0, 1919, 4, 1079); 6]), None);
        // Everything dark: picture would shrink below 40 %.
        assert_eq!(decide(FHD, &[b(800, 1100, 450, 600); 6]), None);
    }

    #[test]
    fn pillarbox_and_too_few_measurements() {
        let m = vec![b(240, 1679, 0, 1079); 6];
        assert_eq!(
            decide(FHD, &m),
            Some(CropRect {
                width: 1440,
                height: 1080,
                x: 240,
                y: 0
            })
        );
        let mut sparse = vec![None; 6];
        sparse[0] = b(240, 1679, 0, 1079);
        sparse[1] = b(240, 1679, 0, 1079);
        assert_eq!(decide(FHD, &sparse), None);
    }

    #[test]
    fn points_skip_intro_and_credits() {
        let p = points(6000.0);
        assert_eq!(p.len(), POINTS as usize);
        assert!(p.iter().all(|t| (600.0..=5400.0).contains(t)));
        assert!(points(0.0).is_empty());
    }
}
