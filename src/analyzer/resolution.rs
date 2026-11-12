//! Target resolution calculation.
//!
//! Rules (see docs/ARCHITECTURE.md):
//! * sources larger than the bounding box are scaled down to fit,
//! * sources smaller than the box are **never** upscaled,
//! * aspect ratio is always preserved (no padding, no distortion),
//! * odd dimensions are made even (required for 4:2:0) – by cropping a single
//!   line when no scaling happens, so no resampling is introduced,
//! * detected black bars ([`CropRect`]) are cut off first; the rules above
//!   then apply to the remaining picture.

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

/// Picture area without black bars, in displayed (rotated) source pixels.
/// Offsets and size are even, as 4:2:0 chroma requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CropRect {
    pub width: u32,
    pub height: u32,
    pub x: u32,
    pub y: u32,
}

impl CropRect {
    pub fn dimensions(self) -> Dimensions {
        Dimensions::new(self.width, self.height)
    }

    /// Non-empty and inside a frame of size `source`.
    pub fn fits(self, source: Dimensions) -> bool {
        self.width > 0
            && self.height > 0
            && self.x + self.width <= source.width
            && self.y + self.height <= source.height
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
    /// Black bars cut off before scaling (`None` = full frame).
    #[serde(default)]
    pub crop: Option<CropRect>,
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
            crop: None,
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
        return TargetResolution {
            source,
            crop: None,
            output,
            op,
        };
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
        crop: None,
        output,
        op: ResizeOp::Downscale,
    }
}

/// Like [`compute_target`], but cuts off `crop` first. A crop that does not
/// fit the source is ignored.
pub fn compute_target_cropped(
    source: Dimensions,
    crop: Option<CropRect>,
    max: Dimensions,
) -> TargetResolution {
    let Some(crop) = crop.filter(|c| c.fits(source) && c.dimensions() != source) else {
        return compute_target(source, max);
    };
    TargetResolution {
        source,
        crop: Some(crop),
        ..compute_target(crop.dimensions(), max)
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
    fn crop_applies_before_the_bounding_box() {
        let max = Dimensions::new(1920, 1080);
        // UHD scope film with letterbox: 3840×1600 picture inside 3840×2160.
        let crop = CropRect {
            width: 3840,
            height: 1600,
            x: 0,
            y: 280,
        };
        let t = compute_target_cropped(Dimensions::new(3840, 2160), Some(crop), max);
        assert_eq!(t.crop, Some(crop));
        assert_eq!(t.source, Dimensions::new(3840, 2160));
        assert_eq!(
            (t.output, t.op),
            (Dimensions::new(1920, 800), ResizeOp::Downscale)
        );
        // 1080p letterbox: crop only, no scaling.
        let crop = CropRect {
            width: 1920,
            height: 800,
            x: 0,
            y: 140,
        };
        let t = compute_target_cropped(Dimensions::new(1920, 1080), Some(crop), max);
        assert_eq!(
            (t.output, t.op),
            (Dimensions::new(1920, 800), ResizeOp::Keep)
        );
    }

    #[test]
    fn invalid_or_full_frame_crop_is_ignored() {
        let max = Dimensions::new(1920, 1080);
        let source = Dimensions::new(1920, 1080);
        let outside = CropRect {
            width: 1920,
            height: 800,
            x: 0,
            y: 400,
        };
        let full = CropRect {
            width: 1920,
            height: 1080,
            x: 0,
            y: 0,
        };
        for c in [outside, full] {
            assert_eq!(compute_target_cropped(source, Some(c), max).crop, None);
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
