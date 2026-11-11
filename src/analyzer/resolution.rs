//! Target resolution calculation.
//!
//! Rules (see docs/ARCHITECTURE.md):
//! * sources larger than the bounding box are scaled down to fit,
//! * sources smaller than the box are **never** upscaled,
//! * aspect ratio is always preserved (no padding, no distortion),
//! * odd dimensions are made even (required for 4:2:0) – by cropping a single
//!   line when no scaling happens, so no resampling is introduced.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dimensions {
    pub width: u32,
    pub height: u32,
}

impl Dimensions {
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    pub fn pixels(self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }
}

impl std::fmt::Display for Dimensions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}×{}", self.width, self.height)
    }
}

/// How the output size is obtained from the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResizeOp {
    /// Keep the source size as-is.
    Keep,
    /// Crop one row/column to get even dimensions (`crop=w:h:0:0`).
    CropToEven,
    /// Downscale to fit the bounding box.
    Downscale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetResolution {
    pub source: Dimensions,
    pub output: Dimensions,
    pub op: ResizeOp,
}

fn round_even(value: f64) -> u32 {
    let v = ((value / 2.0).round() * 2.0) as u32;
    v.max(2)
}

/// Effective (displayed) source size after applying rotation metadata –
/// ffmpeg auto-rotates, so the bounding box applies to the rotated frame.
pub fn oriented(width: u32, height: u32, rotation: Option<i32>) -> Dimensions {
    match rotation.map(|r| r.rem_euclid(360)) {
        Some(90) | Some(270) => Dimensions::new(height, width),
        _ => Dimensions::new(width, height),
    }
}

/// Compute the output size for `source` within the `max` bounding box.
pub fn compute_target(source: Dimensions, max: Dimensions) -> TargetResolution {
    let (w, h) = (f64::from(source.width), f64::from(source.height));
    let scale = (f64::from(max.width) / w).min(f64::from(max.height) / h);

    if source.width == 0 || source.height == 0 {
        return TargetResolution {
            source,
            output: source,
            op: ResizeOp::Keep,
        };
    }

    if scale >= 1.0 {
        let output = Dimensions::new(source.width & !1, source.height & !1);
        let op = if output == source {
            ResizeOp::Keep
        } else {
            ResizeOp::CropToEven
        };
        return TargetResolution { source, output, op };
    }

    // Keep the limiting dimension exact, derive the other one.
    let output = if f64::from(max.width) / w <= f64::from(max.height) / h {
        Dimensions::new(
            max.width & !1,
            round_even(h * f64::from(max.width & !1) / w),
        )
    } else {
        Dimensions::new(
            round_even(w * f64::from(max.height & !1) / h),
            max.height & !1,
        )
    };
    TargetResolution {
        source,
        output,
        op: ResizeOp::Downscale,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fhd(w: u32, h: u32) -> (Dimensions, ResizeOp) {
        let t = compute_target(Dimensions::new(w, h), Dimensions::new(1920, 1080));
        (t.output, t.op)
    }

    #[test]
    fn specification_examples() {
        assert_eq!(
            fhd(3840, 2160),
            (Dimensions::new(1920, 1080), ResizeOp::Downscale)
        );
        assert_eq!(
            fhd(2560, 1440),
            (Dimensions::new(1920, 1080), ResizeOp::Downscale)
        );
        assert_eq!(
            fhd(1920, 1080),
            (Dimensions::new(1920, 1080), ResizeOp::Keep)
        );
        assert_eq!(fhd(1280, 720), (Dimensions::new(1280, 720), ResizeOp::Keep));
        assert_eq!(fhd(720, 576), (Dimensions::new(720, 576), ResizeOp::Keep));
    }

    #[test]
    fn keeps_aspect_ratio_without_padding() {
        // Cinemascope UHD
        assert_eq!(fhd(3840, 1600).0, Dimensions::new(1920, 800));
        // DCI 4K
        assert_eq!(fhd(4096, 2160).0, Dimensions::new(1920, 1012));
        // 4:3 taller than box
        assert_eq!(fhd(2880, 2160).0, Dimensions::new(1440, 1080));
        // portrait phone video
        assert_eq!(fhd(2160, 3840).0, Dimensions::new(608, 1080));
        // already fits
        assert_eq!(fhd(1920, 800).0, Dimensions::new(1920, 800));
    }

    #[test]
    fn odd_dimensions() {
        assert_eq!(
            fhd(719, 479),
            (Dimensions::new(718, 478), ResizeOp::CropToEven)
        );
        let (d, op) = fhd(3839, 2159);
        assert_eq!(op, ResizeOp::Downscale);
        assert!(d.width % 2 == 0 && d.height % 2 == 0);
    }

    #[test]
    fn never_upscales() {
        for (w, h) in [(320, 240), (640, 360), (1024, 576), (1918, 1080)] {
            let (d, _) = fhd(w, h);
            assert!(d.width <= w && d.height <= h);
        }
    }

    #[test]
    fn custom_box_and_rotation() {
        let t = compute_target(Dimensions::new(1920, 1080), Dimensions::new(1280, 720));
        assert_eq!(t.output, Dimensions::new(1280, 720));
        assert_eq!(oriented(1920, 1080, Some(-90)), Dimensions::new(1080, 1920));
        assert_eq!(oriented(1920, 1080, Some(180)), Dimensions::new(1920, 1080));
    }
}
