//! Hard-cut detection over a sequence of per-frame luma histograms.
//!
//! **Distance is 1D Earth Mover's Distance, not raw histogram overlap.** Two
//! delta-function histograms with no overlapping bins (a solid dark frame vs
//! a solid light one) have *zero* bin overlap regardless of whether they're
//! adjacent tones or opposite ends of the range — a plain intersection/L1
//! metric on the raw bins can't tell "slightly different" from "completely
//! different" in that case. EMD fixes this cheaply for 1D histograms: it's
//! exactly the L1 distance between the two **cumulative** distributions,
//! computed in one pass, and it naturally grows with how far apart the
//! tonal content actually sits.
//!
//! **The cut threshold is adaptive (mean + k·stddev of the clip's own
//! frame-to-frame distances), not fixed.** A fixed threshold tuned to catch
//! a slow dissolve would also fire on ordinary motion in a static clip; one
//! tuned to ignore a dissolve would miss real cuts of similar magnitude in a
//! busier one. Measuring against the clip's own distribution of frame-to-
//! frame change is what lets a hard cut stand out from whatever "normal"
//! looks like for that specific piece of footage.

use crate::scopes::Histogram;

/// 1D Earth Mover's Distance between two luma histograms' cumulative
/// distributions, normalised to 0..1 (0 = identical, 1 = maximally
/// different — one histogram entirely at luma 0, the other entirely at 255).
/// An empty histogram (no pixels) has no claim to make about difference, so
/// it's treated as distance 0 rather than dividing by zero.
pub fn histogram_distance(a: &Histogram, b: &Histogram) -> f64 {
    let total_a: u32 = a.luma.iter().sum();
    let total_b: u32 = b.luma.iter().sum();
    if total_a == 0 || total_b == 0 {
        return 0.0;
    }
    let mut cdf_a = 0f64;
    let mut cdf_b = 0f64;
    let mut area = 0f64;
    for i in 0..256 {
        cdf_a += a.luma[i] as f64 / total_a as f64;
        cdf_b += b.luma[i] as f64 / total_b as f64;
        area += (cdf_a - cdf_b).abs();
    }
    // Max possible area is 255 (one distribution entirely at bin 0, the
    // other entirely at bin 255 — every one of the 255 gaps between bins
    // contributes a full unit of CDF difference).
    (area / 255.0).min(1.0)
}

pub struct CutDetectorConfig {
    /// Cuts are frame-to-frame distances more than this many standard
    /// deviations above the clip's own mean distance.
    pub threshold_multiplier: f64,
}

impl Default for CutDetectorConfig {
    fn default() -> Self {
        CutDetectorConfig { threshold_multiplier: 2.5 }
    }
}

/// Indices into `frames` where a hard cut occurs — each index is the first
/// frame *of the new shot*, matching how a human would describe "the cut is
/// at frame N". Empty for fewer than two frames; never panics.
pub fn detect_cuts(frames: &[Histogram], config: &CutDetectorConfig) -> Vec<usize> {
    if frames.len() < 2 {
        return Vec::new();
    }
    let distances: Vec<f64> =
        frames.windows(2).map(|pair| histogram_distance(&pair[0], &pair[1])).collect();

    let mean = distances.iter().sum::<f64>() / distances.len() as f64;
    let variance =
        distances.iter().map(|d| (d - mean).powi(2)).sum::<f64>() / distances.len() as f64;
    let stddev = variance.sqrt();
    let threshold = mean + config.threshold_multiplier * stddev;

    distances
        .iter()
        .enumerate()
        .filter(|(_, &d)| d > threshold)
        .map(|(i, _)| i + 1)
        .collect()
}
