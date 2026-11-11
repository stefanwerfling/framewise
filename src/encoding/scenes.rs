//! Source complexity from packet sizes and the choice of sample windows.
//!
//! The encoder that produced the source spent more bits where the picture is
//! hard to compress (motion, grain, detail). Packet sizes per second are
//! therefore a cheap complexity map: ffprobe only demuxes, nothing is
//! decoded. Samples taken from the hardest scenes make the quality floor
//! hold for the whole film instead of for three random moments.

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use super::command::Window;
use super::process::ProcessHandle;
use crate::core::error::{Error, Result};
use crate::core::tools::run_capture;

/// Video bytes per second of the source.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Complexity {
    pub bytes_per_s: Vec<u64>,
}

impl Complexity {
    /// Parses `ffprobe -show_entries packet=pts_time,dts_time,size -of csv=p=0`
    /// (fields in ffprobe's order: pts_time, dts_time, size).
    pub fn parse(csv: &str) -> Self {
        let mut bytes_per_s: Vec<u64> = Vec::new();
        for line in csv.lines() {
            let fields: Vec<&str> = line.trim().trim_end_matches(',').split(',').collect();
            if fields.len() < 3 {
                continue;
            }
            let time = fields[0]
                .parse::<f64>()
                .or_else(|_| fields[1].parse::<f64>())
                .ok()
                .filter(|t| t.is_finite() && *t >= 0.0);
            let size = fields[fields.len() - 1].parse::<u64>().ok();
            if let (Some(t), Some(size)) = (time, size) {
                let sec = t as usize;
                if sec >= bytes_per_s.len() {
                    bytes_per_s.resize(sec + 1, 0);
                }
                bytes_per_s[sec] += size;
            }
        }
        Self { bytes_per_s }
    }

    pub fn is_usable(&self) -> bool {
        self.bytes_per_s.len() >= 10 && self.bytes_per_s.iter().any(|b| *b > 0)
    }

    /// Source video bytes inside a window (partial seconds pro rata).
    pub fn bytes_in(&self, w: Window) -> u64 {
        let (start, end) = (w.start_s.max(0.0), (w.start_s + w.duration_s).max(0.0));
        let mut total = 0.0;
        let first = start.floor() as usize;
        let last = (end.ceil() as usize).min(self.bytes_per_s.len());
        for sec in first..last {
            let s = sec as f64;
            let overlap = (end.min(s + 1.0) - start.max(s)).clamp(0.0, 1.0);
            total += self.bytes_per_s[sec] as f64 * overlap;
        }
        total.round() as u64
    }

    /// `count` non-overlapping windows of `seconds` length: about half from
    /// the most complex parts, the rest spread over the film as typical
    /// content. Intro and credits (5 % at each end) are left out. Returns
    /// windows sorted by start time; empty if the film is too short.
    pub fn pick_windows(&self, count: u32, seconds: u32, duration_s: f64) -> Vec<Window> {
        let len = f64::from(seconds.max(2));
        let count = count.max(1) as usize;
        let margin = (duration_s * 0.05).max(len);
        let mut slots: Vec<(Window, u64)> = Vec::new();
        let mut start = margin;
        while start + len <= duration_s - margin {
            let w = Window {
                start_s: start,
                duration_s: len,
            };
            slots.push((w, self.bytes_in(w)));
            start += len;
        }
        if slots.len() < count * 2 {
            return Vec::new();
        }
        let n = slots.len();
        // Keep picks apart so the hard samples do not all come from one
        // action sequence.
        let spacing = (n / (count * 4)).max(2);
        let mut chosen: Vec<usize> = Vec::new();
        let free = |chosen: &[usize], i: usize| chosen.iter().all(|c| c.abs_diff(i) >= spacing);

        let hard = count.div_ceil(2);
        let mut by_bytes: Vec<usize> = (0..n).collect();
        by_bytes.sort_by(|a, b| slots[*b].1.cmp(&slots[*a].1).then(a.cmp(b)));
        for i in by_bytes {
            if chosen.len() >= hard {
                break;
            }
            if free(&chosen, i) {
                chosen.push(i);
            }
        }
        let typical = count - chosen.len();
        for k in 0..typical {
            let target = n * (2 * k + 1) / (2 * typical);
            let pick = (0..n)
                .filter(|i| free(&chosen, *i))
                .min_by_key(|i| i.abs_diff(target));
            if let Some(i) = pick {
                chosen.push(i);
            }
        }
        chosen.sort_unstable();
        chosen.into_iter().map(|i| slots[i].0).collect()
    }
}

/// Reads the packet sizes of video stream `v:<type_index>`. Reads the whole
/// file (I/O bound, no decoding); cancellable through `handle`.
pub async fn measure(
    ffprobe: &Path,
    file: &Path,
    type_index: u32,
    handle: &ProcessHandle,
) -> Result<Complexity> {
    let args: Vec<OsString> = vec![
        "-v".into(),
        "error".into(),
        "-select_streams".into(),
        format!("v:{type_index}").into(),
        "-show_entries".into(),
        "packet=pts_time,dts_time,size".into(),
        "-of".into(),
        "csv=p=0".into(),
        "-i".into(),
        file.as_os_str().to_owned(),
    ];
    let run = run_capture(
        "ffprobe",
        ffprobe,
        args,
        Duration::from_secs(30 * 60),
        "scene complexity",
    );
    // Dropping the future kills ffprobe (kill_on_drop).
    let out = tokio::select! {
        out = run => out?,
        () = handle.cancelled() => return Err(Error::Cancelled),
    };
    Ok(Complexity::parse(&String::from_utf8_lossy(&out.stdout)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complexity(seconds: usize, spikes: &[(usize, u64)]) -> Complexity {
        let mut bytes_per_s = vec![1000; seconds];
        for (at, b) in spikes {
            bytes_per_s[*at] = *b;
        }
        Complexity { bytes_per_s }
    }

    #[test]
    fn parses_packets_with_missing_pts() {
        let c =
            Complexity::parse("0.000000,0.000000,500\nN/A,0.500000,300,\n1.200000,N/A,100\nbad\n");
        assert_eq!(c.bytes_per_s, vec![800, 100]);
    }

    #[test]
    fn bytes_in_counts_partial_seconds() {
        let c = Complexity {
            bytes_per_s: vec![100, 200, 300],
        };
        let w = Window {
            start_s: 0.5,
            duration_s: 2.0,
        };
        assert_eq!(c.bytes_in(w), 50 + 200 + 150);
    }

    #[test]
    fn picks_hard_scenes_and_spreads_the_rest() {
        // 100 min film, two very complex scenes.
        let c = complexity(6000, &[(1205, 900_000), (4502, 800_000)]);
        let windows = c.pick_windows(8, 10, 6000.0);
        assert_eq!(windows.len(), 8);
        let covers = |t: f64| {
            windows
                .iter()
                .any(|w| w.start_s <= t && t < w.start_s + w.duration_s)
        };
        assert!(covers(1205.0) && covers(4502.0));
        // No intro/credits, no overlaps, sorted.
        assert!(
            windows
                .iter()
                .all(|w| w.start_s >= 300.0 && w.start_s + w.duration_s <= 5700.0)
        );
        assert!(
            windows
                .windows(2)
                .all(|p| p[0].start_s + p[0].duration_s <= p[1].start_s)
        );
        // Spread: the typical ones cover early and late parts too.
        assert!(windows.first().unwrap().start_s < 2000.0);
        assert!(windows.last().unwrap().start_s > 4000.0);
    }

    #[test]
    fn short_or_empty_sources_fall_back() {
        let c = complexity(60, &[]);
        assert!(c.pick_windows(8, 10, 60.0).is_empty());
        assert!(!Complexity::default().is_usable());
    }
}
