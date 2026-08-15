//! Motion estimation and path smoothing for Warp Stabilizer.
//!
//! **Block matching, not phase correlation or optical flow.** Given a
//! downsampled greyscale frame pair, `estimate_translation` searches every
//! candidate `(dx, dy)` in a bounded window and picks the one with the
//! lowest sum-of-absolute-differences over the overlapping region — the
//! classic, simple motion-estimation technique (the same idea codec motion
//! estimation uses), not a phase-correlation or feature-tracking approach. It
//! finds one dominant 2D translation per frame pair, which is real, useful
//! stabilization for ordinary handheld shake, and honestly does **not**
//! model rotation, scale, or perspective the way a full Warp Stabilizer
//! does — see `render::stabilize`'s module doc for the fuller scope note.

use render::stabilize::{estimate_translation, stabilization_offsets, MotionConfig};

const W: usize = 64;
const H: usize = 64;

/// A deterministic pseudo-random greyscale texture — needs real, varied
/// content for block matching to have anything to lock onto; a blank or
/// smoothly-varying image has no unique features and would match equally
/// well (or badly) at every offset.
fn textured_frame(seed: u32) -> Vec<f32> {
    let mut state = seed;
    (0..W * H)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 16) as f32 / 65535.0
        })
        .collect()
}

/// Shifts `frame` by `(dx, dy)` pixels, filling revealed edges with the
/// nearest in-bounds pixel (so the overlap region block matching actually
/// searches is genuinely a shifted copy, not shifted-plus-a-black-border
/// that would itself bias the match).
fn shift(frame: &[f32], dx: i32, dy: i32) -> Vec<f32> {
    let mut out = vec![0.0; W * H];
    for y in 0..H as i32 {
        for x in 0..W as i32 {
            let sx = (x - dx).clamp(0, W as i32 - 1);
            let sy = (y - dy).clamp(0, H as i32 - 1);
            out[(y as usize) * W + x as usize] = frame[(sy as usize) * W + sx as usize];
        }
    }
    out
}

fn default_config() -> MotionConfig {
    MotionConfig { max_offset: 12, downsample: 1 }
}

#[test]
fn identical_frames_have_zero_motion() {
    let frame = textured_frame(1);
    let (dx, dy) = estimate_translation(&frame, &frame, W, H, &default_config());
    assert_eq!((dx, dy), (0.0, 0.0));
}

#[test]
fn a_known_shift_is_recovered_exactly() {
    let a = textured_frame(2);
    for &(dx, dy) in &[(3, 0), (0, -4), (5, 5), (-6, 2)] {
        let b = shift(&a, dx, dy);
        let (found_dx, found_dy) = estimate_translation(&a, &b, W, H, &default_config());
        assert_eq!(
            (found_dx, found_dy),
            (dx as f64, dy as f64),
            "shifting by ({dx},{dy}) should be recovered exactly"
        );
    }
}

#[test]
fn a_shift_beyond_the_search_window_clamps_to_the_window_edge_not_a_random_answer() {
    let config = MotionConfig { max_offset: 5, downsample: 1 };
    let a = textured_frame(3);
    let b = shift(&a, 20, 0); // far outside a ±5 search window
    let (dx, _) = estimate_translation(&a, &b, W, H, &config);
    assert_eq!(dx, 5.0, "should saturate at the search window's edge, not report something arbitrary");
}

#[test]
fn a_blank_frame_pair_reports_zero_motion_rather_than_a_random_tie() {
    // With no texture, every candidate offset matches equally well (SAD=0
    // everywhere) — the tie-break must be deterministic (favouring zero
    // motion) rather than picking whatever the search order happens to visit
    // first, which would make stabilization jitter on a static, texture-free
    // shot for no reason.
    let blank = vec![0.5f32; W * H];
    let (dx, dy) = estimate_translation(&blank, &blank, W, H, &default_config());
    assert_eq!((dx, dy), (0.0, 0.0));
}

// ---- path smoothing / stabilization offsets ----

#[test]
fn a_perfectly_steady_camera_needs_no_correction() {
    let raw_path = vec![(0.0, 0.0); 30];
    let offsets = stabilization_offsets(&raw_path, 5);
    assert!(offsets.iter().all(|&(x, y)| x.abs() < 1e-9 && y.abs() < 1e-9));
}

#[test]
fn a_smooth_pan_needs_no_correction_away_from_the_clips_own_edges() {
    // A steady, deliberate pan (constant velocity) is exactly what
    // stabilization must leave alone — a symmetric moving average of a
    // constant-velocity sequence returns the same sequence, so the
    // correction is ~zero wherever a full symmetric window is available.
    //
    // Excludes the first/last `half` samples deliberately: there, the
    // window is clamped to whatever history exists (documented on
    // `stabilization_offsets`), which is asymmetric for a rising sequence
    // and so has a real, expected nonzero offset — not a bug, just less
    // context at the very start/end of a clip than in its middle.
    let raw_path: Vec<(f64, f64)> = (0..30).map(|i| (i as f64 * 2.0, 0.0)).collect();
    let smoothing_window = 5;
    let half = smoothing_window / 2;
    let offsets = stabilization_offsets(&raw_path, smoothing_window);
    for &(x, y) in &offsets[half..offsets.len() - half] {
        assert!(x.abs() < 1e-9, "a steady pan should need no horizontal correction away from the edges, got {x}");
        assert!(y.abs() < 1e-9);
    }
}

#[test]
fn jitter_on_top_of_a_pan_is_corrected_toward_the_smooth_trend() {
    // A pan with high-frequency shake riding on it — the corrected path
    // (raw + offset) must be measurably smoother (lower frame-to-frame
    // variance) than the raw path, which is stabilization's entire point.
    let raw_path: Vec<(f64, f64)> = (0..40)
        .map(|i| {
            let trend = i as f64 * 2.0;
            let jitter = if i % 2 == 0 { 8.0 } else { -8.0 };
            (trend + jitter, 0.0)
        })
        .collect();
    let offsets = stabilization_offsets(&raw_path, 7);
    let corrected: Vec<f64> = raw_path.iter().zip(&offsets).map(|(&(x, _), &(ox, _))| x + ox).collect();

    let jaggedness = |path: &[f64]| -> f64 {
        path.windows(2).map(|w| (w[1] - w[0]).abs()).sum::<f64>() / (path.len() - 1) as f64
    };
    let raw_x: Vec<f64> = raw_path.iter().map(|&(x, _)| x).collect();
    assert!(
        jaggedness(&corrected) < jaggedness(&raw_x) / 2.0,
        "corrected path should be much smoother frame-to-frame: raw {:.2} vs corrected {:.2}",
        jaggedness(&raw_x),
        jaggedness(&corrected)
    );
}

#[test]
fn offsets_and_path_are_the_same_length() {
    let raw_path = vec![(1.0, 1.0), (2.0, 0.0), (0.0, 3.0)];
    assert_eq!(stabilization_offsets(&raw_path, 5).len(), raw_path.len());
}

#[test]
fn an_empty_or_single_point_path_produces_no_panic() {
    assert!(stabilization_offsets(&[], 5).is_empty());
    let one = stabilization_offsets(&[(1.0, 2.0)], 5);
    assert_eq!(one, vec![(0.0, 0.0)]);
}
