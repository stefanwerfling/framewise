//! Display formatting helpers (locale independent numbers).

pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1000.0 && unit < UNITS.len() - 1 {
        v /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else if v >= 100.0 {
        format!("{v:.0} {}", UNITS[unit])
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

pub fn opt_bytes(n: Option<u64>) -> String {
    n.map_or_else(|| "–".into(), bytes)
}

pub fn bitrate(bps: Option<u64>) -> String {
    match bps {
        Some(b) if b >= 1_000_000 => format!("{:.1} Mb/s", b as f64 / 1e6),
        Some(b) => format!("{} kb/s", b / 1000),
        None => "–".into(),
    }
}

pub fn duration(seconds: Option<f64>) -> String {
    match seconds {
        Some(s) if s.is_finite() && s >= 0.0 => {
            let s = s.round() as u64;
            format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
        }
        _ => "–".into(),
    }
}

pub fn timestamp(unix: i64) -> String {
    // Simple UTC formatting without a date library.
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        secs / 3600,
        (secs / 60) % 60
    )
}

/// Howard Hinnant's days → civil date algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(1_500_000), "1.5 MB");
        assert_eq!(bytes(250_000_000_000), "250 GB");
        assert_eq!(bitrate(Some(8_500_000)), "8.5 Mb/s");
        assert_eq!(bitrate(Some(192_000)), "192 kb/s");
        assert_eq!(duration(Some(3725.4)), "1:02:05");
        assert_eq!(timestamp(0), "1970-01-01 00:00");
        assert_eq!(timestamp(1_700_000_000), "2023-11-14 22:13");
    }
}
