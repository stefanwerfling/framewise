//! Parser for ffmpeg's machine readable progress (`-progress pipe:1`).
//!
//! ffmpeg emits blocks of `key=value` lines terminated by
//! `progress=continue` or `progress=end`. Progress is computed from the
//! encoded timestamp relative to the source duration – never from file sizes.

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProgressUpdate {
    pub frame: Option<u64>,
    pub fps: Option<f64>,
    pub out_time_s: Option<f64>,
    pub total_size: Option<u64>,
    /// Encoding speed relative to real time (`1.5x`).
    pub speed: Option<f64>,
    pub bitrate_kbps: Option<f64>,
    pub ended: bool,
}

impl ProgressUpdate {
    /// Fraction 0..=1 relative to `duration_s`.
    pub fn fraction(&self, duration_s: Option<f64>) -> Option<f32> {
        if self.ended {
            return Some(1.0);
        }
        let d = duration_s.filter(|d| *d > 0.0)?;
        let t = self.out_time_s?;
        Some((t / d).clamp(0.0, 1.0) as f32)
    }

    /// Remaining seconds from encoded position and speed.
    pub fn eta_s(&self, duration_s: Option<f64>) -> Option<f64> {
        let d = duration_s?;
        let t = self.out_time_s?;
        let speed = self.speed.filter(|s| *s > 0.01)?;
        Some(((d - t).max(0.0)) / speed)
    }
}

#[derive(Debug, Default)]
pub struct ProgressParser {
    current: ProgressUpdate,
}

impl ProgressParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one line; returns a complete update at the end of each block.
    pub fn push_line(&mut self, line: &str) -> Option<ProgressUpdate> {
        let (key, value) = line.trim().split_once('=')?;
        let value = value.trim();
        let num = || {
            value
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite() && *v >= 0.0)
        };
        match key {
            "frame" => self.current.frame = value.parse().ok(),
            "fps" => self.current.fps = num(),
            // out_time_us is microseconds; out_time_ms is (historically) also
            // microseconds in ffmpeg. Prefer out_time_us, fall back to out_time.
            "out_time_us" => {
                if let Some(us) = num() {
                    self.current.out_time_s = Some(us / 1_000_000.0);
                }
            }
            "out_time" => {
                if self.current.out_time_s.is_none() {
                    self.current.out_time_s = parse_timestamp(value);
                }
            }
            "total_size" => self.current.total_size = value.parse().ok(),
            "speed" => {
                self.current.speed = value.trim_end_matches('x').trim().parse().ok();
            }
            "bitrate" => {
                self.current.bitrate_kbps = value.trim_end_matches("kbits/s").trim().parse().ok();
            }
            "progress" => {
                self.current.ended = value == "end";
                let update = std::mem::take(&mut self.current);
                return Some(update);
            }
            _ => {}
        }
        None
    }
}

/// Parse `HH:MM:SS.micro`.
pub fn parse_timestamp(text: &str) -> Option<f64> {
    let mut parts = text.split(':');
    let h: f64 = parts.next()?.parse().ok()?;
    let m: f64 = parts.next()?.parse().ok()?;
    let s: f64 = parts.next()?.parse().ok()?;
    let total = h * 3600.0 + m * 60.0 + s;
    (total.is_finite() && total >= 0.0).then_some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "frame=240
fps=48.00
stream_0_0_q=28.0
bitrate=1234.5kbits/s
total_size=1048576
out_time_us=10000000
out_time_ms=10000000
out_time=00:00:10.000000
dup_frames=0
drop_frames=0
speed=2.01x
progress=continue
frame=480
fps=N/A
out_time_us=N/A
out_time=00:00:20.500000
speed=N/A
progress=end
";

    #[test]
    fn parses_blocks() {
        let mut p = ProgressParser::new();
        let updates: Vec<_> = SAMPLE.lines().filter_map(|l| p.push_line(l)).collect();
        assert_eq!(updates.len(), 2);
        let first = &updates[0];
        assert_eq!(first.frame, Some(240));
        assert_eq!(first.out_time_s, Some(10.0));
        assert_eq!(first.speed, Some(2.01));
        assert_eq!(first.total_size, Some(1_048_576));
        assert_eq!(first.bitrate_kbps, Some(1234.5));
        assert_eq!(first.fraction(Some(100.0)), Some(0.1));
        assert!((first.eta_s(Some(100.0)).unwrap() - 44.776).abs() < 0.01);
        assert!(!first.ended);

        let last = &updates[1];
        assert_eq!(last.fps, None);
        assert_eq!(last.speed, None);
        assert_eq!(last.out_time_s, Some(20.5));
        assert!(last.ended);
        assert_eq!(last.fraction(None), Some(1.0));
    }

    #[test]
    fn unknown_duration_gives_no_fraction() {
        let u = ProgressUpdate {
            out_time_s: Some(5.0),
            ..Default::default()
        };
        assert_eq!(u.fraction(None), None);
        assert_eq!(u.fraction(Some(0.0)), None);
        assert_eq!(u.fraction(Some(2.0)), Some(1.0));
    }

    #[test]
    fn ignores_garbage() {
        let mut p = ProgressParser::new();
        assert_eq!(p.push_line("no equals sign"), None);
        assert_eq!(p.push_line(""), None);
        assert_eq!(parse_timestamp("-1:00:00"), None);
    }
}
