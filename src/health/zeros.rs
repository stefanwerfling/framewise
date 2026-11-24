//! Zero-filled regions: the typical trace of a damaged copy (aborted copy,
//! file system repair, failing disk). The container header still announces
//! the full length, but from some point on the file holds only `0x00` –
//! ffmpeg stops there and the encode comes out shorter than the source.
//!
//! Compressed video never contains long runs of zeros, so a block of
//! [`BLOCK`] bytes that is entirely zero is a reliable sign. The file is
//! sampled every [`SPACING`] bytes (at most [`MAX_PROBES`] reads); a hit is
//! narrowed down to block precision by bisection. Cost for a 13 GB file:
//! about 220 reads of 64 KiB.

/// Size of one read.
pub const BLOCK: u64 = 64 * 1024;
/// Distance between two probes; regions of this size are always found.
const SPACING: u64 = 64 * 1024 * 1024;
const MAX_PROBES: u64 = 512;
/// Smaller zero runs are ignored (padding, sparse preallocation).
const MIN_REGION: u64 = 4 * 1024 * 1024;

/// Byte range `[start, end)` that contains only zeros (block precision).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZeroRegion {
    pub start: u64,
    pub end: u64,
}

impl ZeroRegion {
    pub fn bytes(&self) -> u64 {
        self.end - self.start
    }

    /// The region reaches (almost) the end of the file; a few MB of index
    /// data may still follow it.
    pub fn reaches_end(&self, size: u64) -> bool {
        size.saturating_sub(self.end) <= (size / 100).max(16 * 1024 * 1024)
    }

    /// Rough play time of a byte position (assumes an even bitrate).
    pub fn time_at(byte: u64, size: u64, duration: f64) -> f64 {
        if size == 0 {
            return 0.0;
        }
        (byte as f64 / size as f64 * duration).clamp(0.0, duration)
    }
}

/// Random access to the file (local or over HTTP).
pub trait ByteSource {
    /// Up to `len` bytes from `pos`; shorter at the end of the file.
    fn read(
        &mut self,
        pos: u64,
        len: u64,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, String>> + Send;
}

/// Probe positions, spread evenly; the last one covers the end of the file.
pub fn probe_positions(size: u64) -> Vec<u64> {
    if size < BLOCK {
        return Vec::new();
    }
    let last = size - BLOCK;
    let step = SPACING.max(size.div_ceil(MAX_PROBES));
    let mut out: Vec<u64> = (0..)
        .map(|i| i * step + step / 2)
        .take_while(|p| *p < last)
        .map(align)
        .collect();
    out.push(last);
    out.dedup();
    out
}

fn align(pos: u64) -> u64 {
    pos / BLOCK * BLOCK
}

/// All zero-filled regions of at least [`MIN_REGION`] bytes.
pub async fn find_zero_regions<S: ByteSource>(
    src: &mut S,
    size: u64,
) -> Result<Vec<ZeroRegion>, String> {
    let probes = probe_positions(size);
    let mut zero = Vec::with_capacity(probes.len());
    for &p in &probes {
        zero.push(is_zero(&src.read(p, BLOCK).await?));
    }

    let mut regions = Vec::new();
    let mut i = 0;
    while i < probes.len() {
        if !zero[i] {
            i += 1;
            continue;
        }
        // Run of zero probes i..j.
        let mut j = i;
        while j + 1 < probes.len() && zero[j + 1] {
            j += 1;
        }
        let start = if i == 0 {
            if is_zero(&src.read(0, BLOCK).await?) {
                0
            } else {
                first_zero(src, 0, probes[0]).await?
            }
        } else {
            first_zero(src, probes[i - 1], probes[i]).await?
        };
        let end = if j + 1 == probes.len() {
            size
        } else {
            first_data(src, probes[j], probes[j + 1]).await?
        };
        let region = ZeroRegion { start, end };
        if region.bytes() >= MIN_REGION {
            regions.push(region);
        }
        i = j + 1;
    }
    Ok(regions)
}

fn is_zero(block: &[u8]) -> bool {
    !block.is_empty() && block.iter().all(|&b| b == 0)
}

/// Block at `data` has data, block at `zero` is zero: start of the zeros.
async fn first_zero<S: ByteSource>(
    src: &mut S,
    mut data: u64,
    mut zero: u64,
) -> Result<u64, String> {
    while zero - data > BLOCK {
        let mid = align(data + (zero - data) / 2);
        if is_zero(&src.read(mid, BLOCK).await?) {
            zero = mid;
        } else {
            data = mid;
        }
    }
    Ok(zero)
}

/// Block at `zero` is zero, block at `data` has data: end of the zeros.
async fn first_data<S: ByteSource>(
    src: &mut S,
    mut zero: u64,
    mut data: u64,
) -> Result<u64, String> {
    while data - zero > BLOCK {
        let mid = align(zero + (data - zero) / 2);
        if is_zero(&src.read(mid, BLOCK).await?) {
            zero = mid;
        } else {
            data = mid;
        }
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory file: data everywhere except the given zero ranges.
    struct Fake {
        size: u64,
        zeros: Vec<(u64, u64)>,
        reads: usize,
    }

    impl ByteSource for Fake {
        async fn read(&mut self, pos: u64, len: u64) -> Result<Vec<u8>, String> {
            self.reads += 1;
            let end = (pos + len).min(self.size);
            Ok((pos..end)
                .map(|b| {
                    let z = self.zeros.iter().any(|(s, e)| b >= *s && b < *e);
                    if z { 0 } else { 0x5a }
                })
                .collect())
        }
    }

    async fn run(size: u64, zeros: &[(u64, u64)]) -> (Vec<ZeroRegion>, usize) {
        let mut f = Fake {
            size,
            zeros: zeros.to_vec(),
            reads: 0,
        };
        let r = find_zero_regions(&mut f, size).await.unwrap();
        (r, f.reads)
    }

    const MB: u64 = 1024 * 1024;

    #[tokio::test]
    async fn intact_file_has_no_region() {
        assert!(run(2000 * MB, &[]).await.0.is_empty());
        assert!(run(1000, &[]).await.0.is_empty());
    }

    #[tokio::test]
    async fn finds_zeros_up_to_the_index_at_the_end() {
        // Like the damaged Feuerkelch copy: data, zeros, 150 KB of cues.
        let size = 13_334_918_078;
        let start = 10_712_000_000;
        let end = size - 150_000;
        let (r, reads) = run(size, &[(start, end)]).await;
        assert_eq!(r.len(), 1);
        assert!(r[0].start >= start && r[0].start - start < BLOCK);
        assert!(r[0].end <= end && end - r[0].end < BLOCK);
        assert!(r[0].reaches_end(size));
        assert!(reads < 260, "{reads} reads");
    }

    #[tokio::test]
    async fn finds_a_hole_in_the_middle_and_one_at_the_end() {
        let size = 4000 * MB;
        let (r, _) = run(size, &[(1000 * MB, 1300 * MB), (3900 * MB, size)]).await;
        assert_eq!(r.len(), 2);
        assert_eq!((r[0].start, r[0].end), (1000 * MB, 1300 * MB));
        assert!(!r[0].reaches_end(size));
        assert_eq!((r[1].start, r[1].end), (3900 * MB, size));
    }

    #[tokio::test]
    async fn zeros_from_the_start_and_small_runs() {
        let (r, _) = run(500 * MB, &[(0, 500 * MB)]).await;
        assert_eq!(
            r,
            vec![ZeroRegion {
                start: 0,
                end: 500 * MB
            }]
        );
        // A probe hits a 1 MB run: below the threshold.
        let p = probe_positions(500 * MB)[2];
        assert!(run(500 * MB, &[(p, p + MB)]).await.0.is_empty());
    }

    #[test]
    fn time_estimate() {
        assert_eq!(ZeroRegion::time_at(500, 1000, 7200.0), 3600.0);
        assert_eq!(ZeroRegion::time_at(5, 0, 7200.0), 0.0);
    }
}
