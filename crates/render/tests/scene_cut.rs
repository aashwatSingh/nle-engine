//! Hard-cut detection: given a sequence of per-frame luma histograms, find
//! the frame indices where the picture changed abruptly enough to be a cut
//! rather than continuous motion or a dissolve.
//!
//! The core algorithm is deliberately decoupled from decoding: it consumes
//! `render::scopes::Histogram` values, which a caller builds from real
//! decoded frames (see `crates/app/src/scene_cut_action.rs` for that wiring,
//! covered separately by a real-footage smoke test — this file tests the
//! detection logic itself against synthetic histogram sequences, which is
//! both faster and lets each test isolate exactly one property of the
//! algorithm).

use render::scene_cut::{detect_cuts, histogram_distance, CutDetectorConfig};
use render::scopes::Histogram;

fn solid_histogram(luma_bin: usize) -> Histogram {
    let mut h = Histogram { red: [0; 256], green: [0; 256], blue: [0; 256], luma: [0; 256] };
    h.luma[luma_bin] = 1000;
    h.red[luma_bin] = 1000;
    h.green[luma_bin] = 1000;
    h.blue[luma_bin] = 1000;
    h
}

fn default_config() -> CutDetectorConfig {
    CutDetectorConfig::default()
}

#[test]
fn identical_histograms_have_zero_distance() {
    let a = solid_histogram(100);
    let b = solid_histogram(100);
    assert_eq!(histogram_distance(&a, &b), 0.0);
}

#[test]
fn completely_different_histograms_have_maximum_distance() {
    let black = solid_histogram(0);
    let white = solid_histogram(255);
    // Normalised distance metrics (this one included) top out at 1.0 for
    // two distributions with disjoint support.
    assert!((histogram_distance(&black, &white) - 1.0).abs() < 1e-6);
}

#[test]
fn distance_grows_with_how_different_the_content_is() {
    let base = solid_histogram(100);
    let near = solid_histogram(110);
    let far = solid_histogram(200);
    let d_near = histogram_distance(&base, &near);
    let d_far = histogram_distance(&base, &far);
    assert!(d_near > 0.0, "any change should register as nonzero distance");
    assert!(d_far > d_near, "a bigger change should measure as a bigger distance: {d_far} vs {d_near}");
}

#[test]
fn a_constant_sequence_has_no_cuts() {
    let frames: Vec<Histogram> = (0..20).map(|_| solid_histogram(128)).collect();
    let cuts = detect_cuts(&frames, &default_config());
    assert!(cuts.is_empty(), "unchanging content must never be flagged as a cut: {cuts:?}");
}

#[test]
fn a_single_abrupt_change_is_detected_at_the_right_frame() {
    // Frames 0..10 are one shot, 10..20 are a completely different one, cut
    // squarely at 10 — `detect_cuts` reports the index of the *second* frame
    // of the pair, the first frame that actually shows the new shot.
    let mut frames: Vec<Histogram> = (0..10).map(|_| solid_histogram(50)).collect();
    frames.extend((0..10).map(|_| solid_histogram(220)));
    let cuts = detect_cuts(&frames, &default_config());
    assert_eq!(cuts, vec![10], "expected exactly one cut at frame 10, got {cuts:?}");
}

#[test]
fn multiple_cuts_are_all_found() {
    let mut frames: Vec<Histogram> = (0..8).map(|_| solid_histogram(30)).collect();
    frames.extend((0..8).map(|_| solid_histogram(150)));
    frames.extend((0..8).map(|_| solid_histogram(255)));
    let cuts = detect_cuts(&frames, &default_config());
    assert_eq!(cuts, vec![8, 16], "expected cuts at 8 and 16, got {cuts:?}");
}

#[test]
fn a_slow_dissolve_is_not_flagged_as_a_hard_cut() {
    // A dissolve moves the content a little every frame, never all at once.
    // The whole reason this uses an adaptive (mean + k*stddev) threshold
    // rather than a fixed one: a fixed threshold tuned to catch this dissolve
    // would also fire on ordinary motion in a static-content clip, and a
    // fixed threshold that ignores this dissolve would miss real cuts of
    // similar per-frame magnitude in a busier clip.
    let frames: Vec<Histogram> = (0..40).map(|i| solid_histogram(50 + i * 4)).collect();
    let cuts = detect_cuts(&frames, &default_config());
    assert!(cuts.is_empty(), "a gradual dissolve must not be flagged as a hard cut, got {cuts:?}");
}

#[test]
fn a_real_cut_is_still_found_inside_an_otherwise_busy_clip() {
    // Mild frame-to-frame jitter (ordinary handheld motion) throughout, plus
    // one genuine cut partway through. The cut must still stand out against
    // the jitter's own distance distribution.
    let mut luma = 100i32;
    let mut frames = Vec::new();
    for i in 0..30 {
        if i == 15 {
            luma = 240; // the cut
        } else {
            luma += if i % 2 == 0 { 3 } else { -3 }; // small back-and-forth jitter
        }
        frames.push(solid_histogram(luma.clamp(0, 255) as usize));
    }
    let cuts = detect_cuts(&frames, &default_config());
    assert_eq!(cuts, vec![15], "expected the one real cut at frame 15 despite jitter, got {cuts:?}");
}

#[test]
fn fewer_than_two_frames_produces_no_cuts_and_does_not_panic() {
    assert!(detect_cuts(&[], &default_config()).is_empty());
    assert!(detect_cuts(&[solid_histogram(100)], &default_config()).is_empty());
}

#[test]
fn a_cut_at_the_very_first_boundary_is_detected() {
    let mut frames: Vec<Histogram> = (0..5).map(|_| solid_histogram(10)).collect();
    frames.extend((0..5).map(|_| solid_histogram(250)));
    let cuts = detect_cuts(&frames, &default_config());
    assert_eq!(cuts, vec![5]);
}
